// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Nombres de carpetas y archivos, carpetas especiales y filtros.
//!
//! Regla central del formato de backup v2:
//! `<salida>/<etiqueta_o_dominio>/<email>/<carpeta…>`, donde cada segmento de
//! carpeta se sanitiza de forma independiente y **nunca** se reinterpreta el
//! separador del servidor de origen. El nombre remoto original se guarda además
//! en el manifiesto, así que la restauración no depende de heurísticas.
//!
//! Los nombres de carpeta del servidor son datos no confiables: se rechazan los
//! segmentos vacíos, `.` y `..` para que un servidor no pueda escribir fuera del
//! directorio base.

use imap_proto::NameAttribute;
use std::path::{Path, PathBuf};

/// Sanitiza un componente de ruta. Devuelve cadena vacía si el componente no
/// aporta nada utilizable (p. ej. `..`), en cuyo caso el llamador lo descarta.
pub fn sanitize_component(raw: &str) -> String {
    let cleaned = raw.trim().replace(['\r', '\n', '\0'], "");
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return String::new();
    }
    let sanitized = sanitize_filename::sanitize(&cleaned);
    let sanitized = sanitized.trim().trim_end_matches('.').trim().to_string();
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        String::new()
    } else {
        sanitized
    }
}

/// Nombre del archivo `.eml`: `<uid>_<message-id saneado>.eml`.
///
/// El recorte se hace por **caracteres**, no por bytes: cortar una cadena UTF-8
/// por byte provoca un panic cuando la posición cae en medio de un carácter
/// multibyte, y un panic dentro del bucle de descarga aborta el backup entero.
pub fn eml_filename(uid: u32, message_id: Option<&str>) -> String {
    let id_part = match message_id {
        Some(msg_id) => {
            let cleaned = msg_id.trim().trim_matches(|c| c == '<' || c == '>');
            let sanitized = sanitize_component(cleaned);
            if sanitized.is_empty() {
                format!("msg_{}", uid)
            } else {
                truncate_chars(&sanitized, 60)
            }
        }
        None => format!("msg_{}", uid),
    };
    format!("{}_{}.eml", uid, id_part)
}

/// Trunca por caracteres (nunca por bytes) sin dejar el resultado vacío.
pub fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect()
}

/// Directorio base de una cuenta: `<base>/<etiqueta_o_dominio>/<email>`.
pub fn account_dir(base: &Path, label_or_domain: &str, email: &str) -> PathBuf {
    let domain = {
        let s = sanitize_component(label_or_domain);
        if s.is_empty() {
            "default".to_string()
        } else {
            s
        }
    };
    let mail = {
        let s = sanitize_component(email);
        if s.is_empty() {
            "cuenta".to_string()
        } else {
            s
        }
    };
    base.join(domain).join(mail)
}

/// Divide un nombre de carpeta remoto en sus componentes jerárquicos.
/// Si no se conoce el delimitador se prueban los habituales (`/`, `.`).
pub fn split_folder_path(remote_name: &str, delimiter: Option<char>) -> Vec<String> {
    let delims: Vec<char> = match delimiter {
        Some(d) if !d.is_ascii_alphanumeric() => vec![d],
        Some(_) => vec!['/', '.'],
        None => vec!['/', '.'],
    };
    remote_name
        .split(|c: char| delims.contains(&c))
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Ruta relativa (segura) para volcar una carpeta remota dentro del directorio de
/// la cuenta, preservando la jerarquía pero con componentes sanitizados.
pub fn folder_relpath(remote_name: &str, delimiter: Option<char>) -> PathBuf {
    let parts = split_folder_path(remote_name, delimiter);
    let mut path = PathBuf::new();
    for part in parts {
        let safe = sanitize_component(&part);
        if !safe.is_empty() {
            path.push(safe);
        }
    }
    if path.as_os_str().is_empty() {
        path.push("INBOX");
    }
    path
}

/// Reconstruye el nombre remoto a partir de la ruta local del backup y, si existe,
/// del nombre original guardado en el manifiesto.
///
/// Sin manifiesto (backups creados por la v1) se aplica una heurística conservadora:
/// solo se descarta el prefijo `<dominio>/<email>/` cuando el segundo componente es
/// realmente un correo. La v1 descartaba cualquier primer segmento que "pareciera
/// un dominio", lo que borraba carpetas legítimas como `compras.com` o `Clientes.es`.
pub fn remote_name_from_relpath(rel: &Path, known_remote: Option<&str>) -> String {
    if let Some(known) = known_remote {
        if !known.trim().is_empty() {
            return known.trim().to_string();
        }
    }

    let mut parts: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(v) => Some(v.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();

    // Si el último componente es un archivo .mbox/.eml, la carpeta es la ruta padre.
    if let Some(last) = parts.last() {
        let lower = last.to_lowercase();
        if lower.ends_with(".mbox") {
            let stem = last[..last.len() - ".mbox".len()].to_string();
            parts.pop();
            parts.push(stem);
        } else if lower.ends_with(".eml") {
            parts.pop();
        }
    }

    if parts.is_empty() {
        return "INBOX".to_string();
    }

    let looks_like_email = |s: &str| s.contains('@') && s.contains('.');
    let looks_like_domain =
        |s: &str| !s.contains('@') && s.contains('.') && s.chars().next().is_some_and(|c| c.is_alphanumeric());

    let mut start = 0usize;
    if parts.len() >= 2 && looks_like_domain(&parts[0]) && looks_like_email(&parts[1]) {
        start = 2;
    } else if looks_like_email(&parts[0]) {
        start = 1;
    }

    let remaining = &parts[start..];
    if remaining.is_empty() {
        "INBOX".to_string()
    } else {
        remaining.join("/")
    }
}

/// Adapta los componentes de una carpeta al delimitador del servidor de destino.
pub fn adapt_to_delimiter(parts: &[String], delimiter: char) -> String {
    let cleaned: Vec<String> = parts
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if cleaned.is_empty() {
        "INBOX".to_string()
    } else {
        cleaned.join(&delimiter.to_string())
    }
}

pub fn is_inbox(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    lower == "inbox" || lower == "bandeja de entrada" || lower == "inbox/bandeja de entrada"
}

/// Normaliza un nombre de carpeta para comparaciones (sin prefijo jerárquico).
fn last_segment(name: &str) -> String {
    name.rsplit(['/', '.'])
        .next()
        .unwrap_or(name)
        .trim()
        .to_lowercase()
}

/// ¿La regla del filtro cubre a esta carpeta?
///
/// Coincide por nombre completo, por último segmento (así `Spam` alcanza
/// `INBOX/Spam` y `[Gmail]/Spam`) o por jerarquía (`INBOX` cubre `INBOX/Sub`).
fn rule_matches(rule: &str, full: &str, leaf: &str) -> bool {
    let rule_full = rule.trim().to_lowercase();
    if rule_full.is_empty() {
        return false;
    }
    if full == rule_full || leaf == last_segment(&rule_full) {
        return true;
    }
    full.starts_with(&format!("{}/", rule_full)) || full.starts_with(&format!("{}.", rule_full))
}

/// Comprueba si una carpeta remota debe procesarse según las listas de inclusión
/// y exclusión. La coincidencia es insensible a mayúsculas y una regla cubre a
/// sus carpetas hijas.
pub fn matches_filter(
    remote_name: &str,
    exclude_folders: &[String],
    include_only_folders: Option<&[String]>,
) -> bool {
    let full = remote_name.to_lowercase();
    let leaf = last_segment(remote_name);

    if exclude_folders
        .iter()
        .any(|rule| rule_matches(rule, &full, &leaf))
    {
        return false;
    }

    match include_only_folders {
        Some(list) if !list.is_empty() => list
            .iter()
            .any(|rule| rule_matches(rule, &full, &leaf)),
        _ => true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpecialUse {
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    All,
    Flagged,
}

impl SpecialUse {
    /// Atributos `\Sent`, `\Drafts`, `\Trash`, `\Junk`, `\Archive`, `\All`, `\Flagged`
    /// (RFC 6154) devueltos por `LIST`.
    pub fn from_attributes(attrs: &[NameAttribute<'_>]) -> Option<Self> {
        for attr in attrs {
            let mapped = match attr {
                NameAttribute::Sent => Some(SpecialUse::Sent),
                NameAttribute::Drafts => Some(SpecialUse::Drafts),
                NameAttribute::Trash => Some(SpecialUse::Trash),
                NameAttribute::Junk => Some(SpecialUse::Junk),
                NameAttribute::Archive => Some(SpecialUse::Archive),
                NameAttribute::All => Some(SpecialUse::All),
                NameAttribute::Flagged => Some(SpecialUse::Flagged),
                _ => None,
            };
            if mapped.is_some() {
                return mapped;
            }
        }
        None
    }

    /// Heurística por nombre para servidores que no anuncian SPECIAL-USE
    /// (Hostinger/cPanel, Dovecot sin `\Sent`, etc.) en español e inglés.
    pub fn from_name(name: &str) -> Option<Self> {
        let leaf = last_segment(name);
        let leaf = leaf.trim_start_matches("inbox");
        let leaf = leaf.trim_matches(|c| c == '/' || c == '.' || c == ' ');
        match leaf {
            "sent" | "enviados" | "elementos enviados" | "correo enviado" | "sent items"
            | "sent messages" | "sent-mail" => Some(SpecialUse::Sent),
            "drafts" | "borradores" | "borrador" => Some(SpecialUse::Drafts),
            "trash" | "papelera" | "eliminados" | "elementos eliminados" | "deleted items"
            | "deleted messages" | "bin" => Some(SpecialUse::Trash),
            "junk" | "spam" | "correo no deseado" | "correo no deseado " | "bulk mail"
            | "junk e-mail" | "no deseado" => Some(SpecialUse::Junk),
            "archive" | "archivo" | "archivos" | "all mail" | "todos" => Some(SpecialUse::Archive),
            "flagged" | "destacados" | "marcados" => Some(SpecialUse::Flagged),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SpecialUse::Sent => "Enviados",
            SpecialUse::Drafts => "Borradores",
            SpecialUse::Trash => "Papelera",
            SpecialUse::Junk => "Spam",
            SpecialUse::Archive => "Archivo",
            SpecialUse::All => "Todos",
            SpecialUse::Flagged => "Destacados",
        }
    }
}

/// true si la carpeta es un buzón que no se puede seleccionar (p. ej. `[Gmail]`).
pub fn is_selectable(attrs: &[NameAttribute<'_>]) -> bool {
    !attrs.iter().any(|a| matches!(a, NameAttribute::NoSelect))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eml_filename_is_stable_and_safe() {
        assert_eq!(eml_filename(42, None), "42_msg_42.eml");
        assert_eq!(
            eml_filename(42, Some("<abc@host.com>")),
            "42_abc@host.com.eml"
        );
        // Caracteres inválidos en Windows se eliminan.
        assert!(!eml_filename(7, Some("<a/b\\c:d>")).contains('/'));
    }

    #[test]
    fn eml_filename_handles_multibyte_message_ids_without_panicking() {
        // 65+ bytes con acentos: el recorte debe ser por caracteres y no por bytes.
        let long_utf8 = format!("<{}@correo.es>", "café".repeat(20));
        assert!(long_utf8.len() > 60);
        let name = eml_filename(1, Some(&long_utf8));
        assert!(name.ends_with(".eml"));
        assert!(name.is_char_boundary(name.len() - 4));
        let stem = name.trim_end_matches(".eml");
        let id_part = stem.split_once('_').unwrap().1;
        assert!(id_part.chars().count() <= 60);
        assert!(std::str::from_utf8(id_part.as_bytes()).is_ok());
    }

    #[test]
    fn eml_filename_falls_back_when_message_id_is_empty() {
        assert_eq!(eml_filename(9, Some("<>")), "9_msg_9.eml");
        assert_eq!(eml_filename(9, Some("   ")), "9_msg_9.eml");
    }

    #[test]
    fn sanitize_component_rejects_traversal_and_separators() {
        assert_eq!(sanitize_component(".."), "");
        assert_eq!(sanitize_component("."), "");
        assert_eq!(sanitize_component(""), "");
        assert_eq!(sanitize_component("../../etc"), "....etc");
        assert_eq!(sanitize_component("a/b"), "ab");
        assert_eq!(sanitize_component("Facturas 2024.01"), "Facturas 2024.01");
    }

    #[test]
    fn account_dir_uses_label_then_email() {
        let dir = account_dir(Path::new("/base"), "midominio.com", "contacto@midominio.com");
        assert!(dir.ends_with("midominio.com/contacto@midominio.com"));
    }

    #[test]
    fn folder_relpath_is_nested_and_sanitized() {
        let rel = folder_relpath("INBOX/Clientes 2024", Some('/'));
        assert_eq!(rel, PathBuf::from("INBOX").join("Clientes 2024"));
        let dotted = folder_relpath("Facturas.Archivo", Some('.'));
        assert_eq!(dotted, PathBuf::from("Facturas").join("Archivo"));
    }

    #[test]
    fn dotted_folder_name_is_not_split_when_delimiter_is_slash() {
        // Un nombre con punto NO debe convertirse en jerarquía si el delimitador es '/'.
        let rel = folder_relpath("Facturas 2024.01", Some('/'));
        assert_eq!(rel, PathBuf::from("Facturas 2024.01"));
    }

    #[test]
    fn remote_name_uses_manifest_when_available() {
        let rel = PathBuf::from("midominio.com").join("a@b.com").join("Enviados");
        assert_eq!(
            remote_name_from_relpath(&rel, Some("INBOX.Sent")),
            "INBOX.Sent"
        );
    }

    #[test]
    fn remote_name_strips_domain_only_when_followed_by_email() {
        let with_prefix = PathBuf::from("midominio.com").join("a@b.com").join("Clientes");
        assert_eq!(remote_name_from_relpath(&with_prefix, None), "Clientes");

        // Carpeta legítima llamada "compras.com" (sin email detrás): se conserva.
        let legit = PathBuf::from("compras.com").join("Facturas");
        assert_eq!(remote_name_from_relpath(&legit, None), "compras.com/Facturas");
    }

    #[test]
    fn remote_name_recovers_dotted_folder_names() {
        let rel = PathBuf::from("a@b.com").join("Facturas 2024.01");
        assert_eq!(remote_name_from_relpath(&rel, None), "Facturas 2024.01");
    }

    #[test]
    fn remote_name_handles_mbox_files() {
        let rel = PathBuf::from("midominio.com")
            .join("a@b.com")
            .join("INBOX")
            .join("Sent.mbox");
        assert_eq!(remote_name_from_relpath(&rel, None), "INBOX/Sent");
    }

    #[test]
    fn filters_match_nested_and_localized_names() {
        let exclude = vec!["Spam".to_string(), "Papelera".to_string()];
        assert!(!matches_filter("[Gmail]/Spam", &exclude, None));
        assert!(!matches_filter("INBOX/Papelera", &exclude, None));
        assert!(matches_filter("INBOX", &exclude, None));

        let include = vec!["INBOX".to_string(), "Sent".to_string()];
        assert!(matches_filter("INBOX/Sub", &[], Some(&include)));
        assert!(matches_filter("INBOX", &[], Some(&include)));
        assert!(!matches_filter("Otros", &[], Some(&include)));
        // Un nombre que solo empieza igual no debe coincidir por prefijo.
        assert!(!matches_filter("INBOXOTRO", &[], Some(&include)));
        // Excluir una carpeta excluye también a sus hijas.
        let parents = vec!["Archivo".to_string()];
        assert!(!matches_filter("Archivo/2024", &parents, None));
        assert!(matches_filter("Archivos", &parents, None));
    }

    #[test]
    fn special_use_from_attributes_and_names() {
        let attrs = vec![NameAttribute::Sent];
        assert_eq!(
            SpecialUse::from_attributes(&attrs),
            Some(SpecialUse::Sent)
        );
        assert_eq!(SpecialUse::from_name("Elementos enviados"), Some(SpecialUse::Sent));
        assert_eq!(SpecialUse::from_name("INBOX.Correo no deseado"), Some(SpecialUse::Junk));
        assert_eq!(SpecialUse::from_name("Proyectos"), None);
        assert_eq!(SpecialUse::from_name("[Gmail]/Todos"), Some(SpecialUse::Archive));
    }

    #[test]
    fn selectable_check_honours_noselect() {
        assert!(!is_selectable(&[NameAttribute::NoSelect]));
        assert!(is_selectable(&[NameAttribute::Sent]));
        assert!(is_selectable(&[]));
    }

    #[test]
    fn adapt_to_delimiter_joins_and_guards_empty() {
        let parts = vec!["INBOX".to_string(), "Sub".to_string()];
        assert_eq!(adapt_to_delimiter(&parts, '.'), "INBOX.Sub");
        assert_eq!(adapt_to_delimiter(&[], '/'), "INBOX");
    }
}
