// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Formato Maildir: un archivo por mensaje dentro de `<carpeta>/{tmp,new,cur}`.
//!
//! El nombre incluye la información del mensaje (`:2,<flags>`; `;2,<flags>` en
//! Windows, donde `:` no es válido en un nombre de archivo), tal como esperan los
//! MUA (Mutt, notmuch, Dovecot, etc.). Se escribe primero en `tmp/` y se mueve a
//! `cur/` con un `rename`, así que nunca queda un mensaje a medias.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub struct MaildirWriter {
    root: PathBuf,
    counter: std::cell::Cell<u64>,
}

impl MaildirWriter {
    /// Crea (si hace falta) `tmp/`, `new/` y `cur/`.
    pub fn open(root: &Path) -> Result<Self> {
        for sub in ["tmp", "new", "cur"] {
            fs::create_dir_all(root.join(sub)).with_context(|| {
                format!("No se pudo crear {}/{}", root.display(), sub)
            })?;
        }
        Ok(Self {
            root: root.to_path_buf(),
            counter: std::cell::Cell::new(0),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Guarda un mensaje y devuelve la ruta final dentro de `cur/`.
    pub fn append(&self, uid: u32, raw: &[u8], flags: &[String]) -> Result<PathBuf> {
        let base = base_name(uid, message_id_of(raw).as_deref());
        let flag_letters = maildir_flags(flags);
        let file_name = filename(&base, &flag_letters);
        let tmp_path = self.root.join("tmp").join(format!(
            "{}.{}",
            file_name.replace(':', "_"),
            std::process::id()
        ));
        let final_path = self.root.join("cur").join(&file_name);

        {
            use std::io::Write;
            let mut file = fs::File::create(&tmp_path)
                .with_context(|| format!("No se pudo crear {}", tmp_path.display()))?;
            file.write_all(raw)?;
            file.flush().ok();
            file.sync_all().ok();
        }
        if final_path.exists() {
            let _ = fs::remove_file(&final_path);
        }
        fs::rename(&tmp_path, &final_path)
            .with_context(|| format!("No se pudo mover a {}", final_path.display()))?;
        self.counter.set(self.counter.get() + 1);
        Ok(final_path)
    }
}

/// Nombre base determinista por mensaje: lo hace reanudable e idempotente.
pub fn base_name(uid: u32, message_id: Option<&str>) -> String {
    let id_part = message_id
        .map(|id| {
            crate::folders::sanitize_component(
                id.trim().trim_matches(|c| c == '<' || c == '>'),
            )
        })
        .filter(|id| !id.is_empty())
        .map(|id| crate::folders::truncate_chars(&id, 60))
        .unwrap_or_else(|| "msg".to_string());
    format!("{}.{}", uid, id_part)
}

/// Separador de la sección de información de Maildir. El estándar usa `:`, pero
/// Windows no admite `:` en un nombre de archivo, así que ahí se escribe `;`
/// (la misma solución que adoptan mutt y Dovecot en ese sistema).
pub const INFO_SEPARATOR: char = if cfg!(windows) { ';' } else { ':' };

/// Construye el nombre completo con la sección de información de Maildir.
pub fn filename(base: &str, flags: &str) -> String {
    format!("{}{}2,{}", base, INFO_SEPARATOR, flags)
}

/// Extrae la base y las letras de flags de un nombre Maildir. Acepta `:` y `;`
/// como separador para leer carpetas escritas por otros programas.
pub fn parse_filename(name: &str) -> (String, String) {
    if let Some(pos) = name.find(":2,").or_else(|| name.find(";2,")) {
        let (base, rest) = name.split_at(pos);
        return (base.to_string(), rest[3..].to_string());
    }
    (name.to_string(), String::new())
}

/// Traduce flags IMAP a letras Maildir: `\Draft`→D, `\Flagged`→F, `\Answered`→R,
/// `\Seen`→S, `\Deleted`→T (RFC 3501 + convención Maildir).
pub fn maildir_flags(imap_flags: &[String]) -> String {
    let mut letters: Vec<char> = Vec::new();
    for flag in imap_flags {
        let letter = match flag.to_lowercase().as_str() {
            "\\draft" => Some('D'),
            "\\flagged" => Some('F'),
            "\\answered" => Some('R'),
            "\\seen" => Some('S'),
            "\\deleted" => Some('T'),
            _ => None,
        };
        if let Some(letter) = letter {
            if !letters.contains(&letter) {
                letters.push(letter);
            }
        }
    }
    letters.sort_unstable();
    letters.into_iter().collect()
}

/// Traduce letras Maildir a flags IMAP.
pub fn imap_flags(letters: &str) -> Vec<String> {
    let mut flags = Vec::new();
    for letter in letters.chars() {
        let flag = match letter {
            'D' => "\\Draft",
            'F' => "\\Flagged",
            'R' => "\\Answered",
            'S' => "\\Seen",
            'T' => "\\Deleted",
            // 'P' (passed) y otras letras no tienen equivalente IMAP.
            _ => continue,
        };
        if !flags.iter().any(|f: &String| f == flag) {
            flags.push(flag.to_string());
        }
    }
    flags
}

/// Lee las flags de un mensaje Maildir a partir de su ruta.
pub fn flags_from_path(path: &Path) -> Vec<String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let (_, letters) = parse_filename(&name);
    imap_flags(&letters)
}

/// Extrae el `Message-ID` de las cabeceras crudas (sin parsear el mensaje completo).
pub fn message_id_of(raw: &[u8]) -> Option<String> {
    const LIMIT: usize = 64 * 1024;
    let head_len = raw.len().min(LIMIT);
    let head = String::from_utf8_lossy(&raw[..head_len]);
    for line in head.lines() {
        if line.trim().is_empty() {
            break; // fin de cabeceras
        }
        if let Some((name, value)) = line.trim_start().split_once(':') {
            if name.eq_ignore_ascii_case("message-id") {
                let cleaned = value.trim().trim_matches(|c| c == '<' || c == '>');
                if !cleaned.is_empty() {
                    return Some(cleaned.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "imapb-maildir-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn flag_mapping_is_bidirectional() {
        let imap = vec![
            "\\Seen".to_string(),
            "\\Flagged".to_string(),
            "\\Answered".to_string(),
        ];
        let letters = maildir_flags(&imap);
        assert_eq!(letters, "FRS");
        let mut back = imap_flags(&letters);
        back.sort();
        let mut expected = imap.clone();
        expected.sort();
        assert_eq!(back, expected);
        assert!(imap_flags("P").is_empty());
    }

    #[test]
    fn filename_and_parse_roundtrip() {
        let name = filename("42_abc@host.com", "FS");
        assert_eq!(
            name,
            format!("42_abc@host.com{}2,FS", INFO_SEPARATOR),
            "el nombre debe ser el estándar Maildir en esta plataforma"
        );
        assert_eq!(filename("base", ""), format!("base{}2,", INFO_SEPARATOR));
        // Al leer se acepta el separador de la otra plataforma.
        assert_eq!(
            parse_filename("42_abc@host.com:2,FS"),
            ("42_abc@host.com".to_string(), "FS".to_string())
        );
        assert_eq!(
            parse_filename("42_abc@host.com;2,FS"),
            ("42_abc@host.com".to_string(), "FS".to_string())
        );
        let (base, flags) = parse_filename(&name);
        assert_eq!(base, "42_abc@host.com");
        assert_eq!(flags, "FS");
        let (base2, flags2) = parse_filename("sin-info");
        assert_eq!(base2, "sin-info");
        assert!(flags2.is_empty());
    }

    #[test]
    fn writer_creates_structure_and_moves_from_tmp_to_cur() {
        let dir = temp_dir("write");
        let writer = MaildirWriter::open(&dir).unwrap();
        let message = b"Subject: Hola\r\nMessage-ID: <abc@host>\r\n\r\ncuerpo\r\n";
        let path = writer
            .append(7, message, &["\\Seen".to_string()])
            .unwrap();
        assert!(path.starts_with(dir.join("cur")));
        assert_eq!(std::fs::read(&path).unwrap(), message);
        assert_eq!(flags_from_path(&path), vec!["\\Seen".to_string()]);
        assert_eq!(fs::read_dir(dir.join("tmp")).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn base_name_uses_sanitized_message_id() {
        assert_eq!(base_name(1, Some("<a/b@c>")), "1.ab@c");
        assert_eq!(base_name(1, None), "1.msg");
        assert_eq!(base_name(1, Some("<>")), "1.msg");
    }

    #[test]
    fn message_id_is_extracted_from_raw_headers() {
        let raw = b"Return-Path: <a@b>\r\nMessage-ID: <xyz@host.com>\r\nSubject: t\r\n\r\nbody Message-ID: no";
        assert_eq!(message_id_of(raw).unwrap(), "xyz@host.com");
        assert!(message_id_of(b"Subject: sin id\r\n\r\ncuerpo").is_none());
        assert!(message_id_of(b"").is_none());
    }
}
