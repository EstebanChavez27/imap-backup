// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Lectura y escritura de mbox en variante **mboxrd**.
//!
//! El formato mbox separa mensajes con una línea `From ` al principio de línea, así
//! que cualquier línea del cuerpo que empiece con `From ` debe escaparse con un `>`
//! adicional, y al leer hay que deshacer ese escape. Sin este detalle, un correo que
//! incluya el texto "From " en su cuerpo se parte en dos al importarlo.
//!
//! El lector **no** carga el archivo completo: primero calcula los desplazamientos de
//! cada mensaje (`scan_offsets`) y después lee solo el mensaje pedido. Un mbox de 4 GB
//! se puede restaurar con memoria constante.

use anyhow::{Context, Result};
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Escritor incremental: mantiene un mensaje abierto por carpeta, así que no hace
/// falta tener todo el buzón en memoria.
pub struct MboxWriter {
    path: PathBuf,
    file: BufWriter<File>,
    messages: u64,
}

impl MboxWriter {
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let file = File::create(path)
            .with_context(|| format!("No se pudo crear el archivo mbox: {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            file: BufWriter::new(file),
            messages: 0,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn messages(&self) -> u64 {
        self.messages
    }

    /// Escribe un mensaje precedido por su línea `From `.
    /// Devuelve la cantidad de bytes del mensaje (sin la línea `From `).
    pub fn append(
        &mut self,
        raw: &[u8],
        from_address: &str,
        date_rfc2822: Option<&str>,
    ) -> Result<u64> {
        let from_line = build_from_line(from_address, date_rfc2822);
        self.file.write_all(from_line.as_bytes())?;
        let written = write_escaped_body(&mut self.file, raw)?;
        // mbox exige una línea vacía como separador entre mensajes.
        self.file.write_all(b"\n")?;
        self.messages += 1;
        Ok(written)
    }

    pub fn finish(mut self) -> Result<PathBuf> {
        self.file.flush()?;
        Ok(self.path)
    }
}

/// Línea separadora `From <dirección> <fecha>`.
pub fn build_from_line(from_address: &str, date_rfc2822: Option<&str>) -> String {
    let address = if from_address.trim().is_empty() {
        "unknown@localhost"
    } else {
        from_address.trim()
    };
    let date = date_rfc2822
        .and_then(parse_message_date)
        .unwrap_or_else(|| chrono::Local::now().fixed_offset())
        .format("%a %b %e %H:%M:%S %Y")
        .to_string();
    format!("From {} {}\n", address, date)
}

/// Interpreta la fecha `Date:` de un mensaje con tolerancia: si el cliente de
/// origen escribió un día de la semana equivocado (frecuente en software antiguo)
/// el estricto RFC 2822 falla, así que se reintenta sin ese prefijo.
pub fn parse_message_date(raw: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(date) = chrono::DateTime::parse_from_rfc2822(raw) {
        return Some(date.fixed_offset());
    }
    let without_weekday = raw
        .split_once(", ")
        .map(|(_, rest)| rest)
        .filter(|rest| !rest.trim().is_empty())
        .unwrap_or(raw);
    if without_weekday != raw {
        if let Ok(date) = chrono::DateTime::parse_from_rfc2822(without_weekday) {
            return Some(date.fixed_offset());
        }
    }
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|date| date.fixed_offset())
        .ok()
}

/// Escribe el cuerpo del mensaje aplicando el escape mboxrd y normalizando los
/// finales de línea a `\n` (los mbox usan LF; el `\r` se recupera al leer).
fn write_escaped_body<W: Write>(writer: &mut W, raw: &[u8]) -> Result<u64> {
    let text = String::from_utf8_lossy(raw);
    let mut written: u64 = 0;
    let mut first = true;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if !first {
            writer.write_all(b"\n")?;
            written += 1;
        }
        first = false;
        let escaped = escape_line(line);
        writer.write_all(escaped.as_bytes())?;
        written += escaped.len() as u64;
    }
    Ok(written)
}

/// Agrega un `>` al principio de cualquier línea que ya empiece con `From ` (con
/// cualquier cantidad de `>` previos), según mboxrd.
pub fn escape_line(line: &str) -> Cow<'_, str> {
    let without_quotes = line.trim_start_matches('>');
    if without_quotes.starts_with("From ") || without_quotes == "From" {
        Cow::Owned(format!(">{}", line))
    } else {
        Cow::Borrowed(line)
    }
}

/// Deshace el escape mboxrd al leer.
pub fn unescape_line(line: &str) -> Cow<'_, str> {
    if let Some(rest) = line.strip_prefix('>') {
        let probes = rest.trim_start_matches('>');
        if probes.starts_with("From ") || probes == "From" {
            return Cow::Owned(rest.to_string());
        }
    }
    Cow::Borrowed(line)
}

/// Garantiza finales de línea CRLF (lo que espera RFC 5322 y toleran todos los
/// servidores IMAP). Se usa al restaurar contenido leído desde mbox/Maildir.
pub fn normalize_crlf(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / 16);
    let mut index = 0usize;
    while index < input.len() {
        let byte = input[index];
        if byte == b'\n' && (index == 0 || input[index - 1] != b'\r') {
            out.extend_from_slice(b"\r\n");
        } else {
            out.push(byte);
        }
        index += 1;
    }
    out
}

/// Desplazamientos `(inicio, fin)` de cada mensaje dentro de un archivo mbox,
/// calculados por streaming (memoria constante).
pub fn scan_offsets(path: &Path) -> Result<Vec<(u64, u64)>> {
    let file = File::open(path)
        .with_context(|| format!("No se pudo abrir el archivo mbox: {}", path.display()))?;
    scan_offsets_reader(BufReader::with_capacity(256 * 1024, file))
        .with_context(|| format!("Error leyendo {}", path.display()))
}

/// Igual que [`scan_offsets`] pero sobre cualquier lector (por ejemplo una entrada
/// mbox comprimida dentro de un ZIP).
pub fn scan_offsets_reader<R: BufRead>(mut reader: R) -> Result<Vec<(u64, u64)>> {
    let mut starts: Vec<u64> = Vec::new();
    let mut offset: u64 = 0;
    let mut line = Vec::new();

    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if line.starts_with(b"From ") {
            starts.push(offset);
        }
        offset += read as u64;
    }

    let mut bounds = Vec::with_capacity(starts.len());
    for (index, start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(offset);
        bounds.push((*start, end));
    }
    Ok(bounds)
}

/// Lector por acceso aleatorio sobre un archivo mbox ya indexado.
pub struct MboxReader {
    path: PathBuf,
    file: File,
    offsets: Vec<(u64, u64)>,
}

impl MboxReader {
    pub fn open(path: &Path) -> Result<Self> {
        let offsets = scan_offsets(path)?;
        Self::open_with_offsets(path, offsets)
    }

    pub fn open_with_offsets(path: &Path, offsets: Vec<(u64, u64)>) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("No se pudo abrir el archivo mbox: {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            offsets,
        })
    }

    pub fn count(&self) -> usize {
        self.offsets.len()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn message_size(&self, index: usize) -> u64 {
        self.offsets
            .get(index)
            .map(|(start, end)| end.saturating_sub(*start))
            .unwrap_or(0)
    }

    /// Devuelve el mensaje `index` sin la línea `From `, con el escape deshecho y
    /// los finales de línea normalizados a CRLF.
    pub fn message(&mut self, index: usize) -> Result<Vec<u8>> {
        let (start, end) = *self
            .offsets
            .get(index)
            .with_context(|| format!("Mensaje {} fuera de rango en {}", index, self.path.display()))?;
        let length = end.saturating_sub(start) as usize;
        if length == 0 {
            return Ok(Vec::new());
        }
        self.file.seek(SeekFrom::Start(start))?;
        let mut buffer = vec![0u8; length];
        self.file.read_exact(&mut buffer)?;
        Ok(decode_stored_message(&buffer))
    }
}

/// Recupera el mensaje original desde el contenido almacenado en mbox: descarta la
/// línea `From `, deshace el escape mboxrd y normaliza a CRLF.
pub fn decode_stored_message(stored: &[u8]) -> Vec<u8> {
    let body = match stored.iter().position(|&b| b == b'\n') {
        Some(newline) => &stored[newline + 1..],
        None => return Vec::new(),
    };
    normalize_crlf(&unescape_body(body))
}

fn unescape_body(body: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(body);
    let mut out = String::with_capacity(text.len());
    let mut first = true;
    for line in text.split('\n') {
        if !first {
            out.push('\n');
        }
        first = false;
        let line = line.strip_suffix('\r').unwrap_or(line);
        out.push_str(&unescape_line(line));
    }
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MESSAGE: &str = "From: remitente@ejemplo.com\r\nTo: destino@ejemplo.com\r\nSubject: Prueba\r\n\r\nHola mundo\r\nFrom el principio\r\n>From ya citado\r\n";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("imapb-mbox-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn escape_and_unescape_are_inverse() {
        assert_eq!(escape_line("From aqui"), ">From aqui");
        assert_eq!(escape_line(">From aqui"), ">>From aqui");
        assert_eq!(escape_line("Fromm"), "Fromm");
        assert_eq!(unescape_line(">From aqui"), "From aqui");
        assert_eq!(unescape_line(">>From aqui"), ">From aqui");
        assert_eq!(unescape_line("texto"), "texto");
        assert_eq!(unescape_line(">cita normal"), ">cita normal");
    }

    #[test]
    fn writer_reader_roundtrip_preserves_body_and_splits_messages() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("INBOX.mbox");
        {
            let mut writer = MboxWriter::create(&path).unwrap();
            writer
                .append(
                    MESSAGE.as_bytes(),
                    "remitente@ejemplo.com",
                    Some("Thu, 2 Jan 2025 03:04:05 +0000"),
                )
                .unwrap();
            writer
                .append(b"Subject: Segundo\r\n\r\nCuerpo dos\r\n", "otro@ejemplo.com", None)
                .unwrap();
            writer.finish().unwrap();
        }

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.starts_with("From remitente@ejemplo.com Thu Jan  2 03:04:05 2025\n"),
            "línea From inesperada: {:?}",
            raw.lines().next().unwrap_or_default()
        );
        assert!(!raw.contains('\r'));
        assert!(raw.contains(">From el principio"));

        let mut reader = MboxReader::open(&path).unwrap();
        assert_eq!(reader.count(), 2);
        let first = String::from_utf8(reader.message(0).unwrap()).unwrap();
        assert!(first.starts_with("From: remitente@ejemplo.com\r\n"));
        assert!(first.contains("Hola mundo\r\n"));
        assert!(first.contains("From el principio"));
        assert!(first.contains(">From ya citado"));
        assert!(!first.contains("\n\n\r"));
        let second = String::from_utf8(reader.message(1).unwrap()).unwrap();
        assert!(second.contains("Cuerpo dos"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_body_line_starting_with_from_does_not_split_the_message() {
        let dir = temp_dir("nosplit");
        let path = dir.join("INBOX.mbox");
        let tricky = "Subject: Trampa\r\n\r\nLinea 1\r\nFrom esto no es un mensaje\r\nLinea 3\r\n";
        {
            let mut writer = MboxWriter::create(&path).unwrap();
            writer.append(tricky.as_bytes(), "a@b.com", None).unwrap();
            writer.finish().unwrap();
        }
        let mut reader = MboxReader::open(&path).unwrap();
        assert_eq!(reader.count(), 1);
        let body = String::from_utf8(reader.message(0).unwrap()).unwrap();
        assert!(body.contains("From esto no es un mensaje"));
        assert!(body.contains("Linea 3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offsets_are_computed_by_streaming_and_match_messages() {
        let dir = temp_dir("offsets");
        let path = dir.join("INBOX.mbox");
        {
            let mut writer = MboxWriter::create(&path).unwrap();
            for index in 0..5 {
                let message = format!("Subject: {}\r\n\r\ncuerpo {}\r\n", index, index);
                writer.append(message.as_bytes(), "a@b.com", None).unwrap();
            }
            writer.finish().unwrap();
        }
        let offsets = scan_offsets(&path).unwrap();
        assert_eq!(offsets.len(), 5);
        let mut reader = MboxReader::open_with_offsets(&path, offsets).unwrap();
        for index in 0..5u32 {
            let body = String::from_utf8(reader.message(index as usize).unwrap()).unwrap();
            assert!(body.contains(&format!("cuerpo {}", index)));
            assert!(reader.message_size(index as usize) > 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_foreign_mbox_with_crlf_and_consecutive_messages() {
        let dir = temp_dir("foreign");
        let path = dir.join("otro.mbox");
        let content = "From a@b.com Mon Jan  2 03:04:05 2025\r\nSubject: Uno\r\n\r\ncuerpo uno\r\nFrom c@d.com Mon Jan  2 03:04:06 2025\r\nSubject: Dos\r\n\r\ncuerpo dos\r\n";
        std::fs::write(&path, content).unwrap();
        let mut reader = MboxReader::open(&path).unwrap();
        assert_eq!(reader.count(), 2);
        assert!(String::from_utf8(reader.message(1).unwrap())
            .unwrap()
            .contains("cuerpo dos"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_file_has_no_messages() {
        let dir = temp_dir("empty");
        let path = dir.join("vacio.mbox");
        std::fs::write(&path, b"").unwrap();
        let reader = MboxReader::open(&path).unwrap();
        assert_eq!(reader.count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_line_survives_wrong_and_odd_dates() {
        // Día de la semana equivocado: se usa la fecha real, no "ahora".
        assert_eq!(
            build_from_line("a@b.com", Some("Mon, 2 Jan 2025 03:04:05 +0000")),
            "From a@b.com Thu Jan  2 03:04:05 2025\n"
        );
        assert_eq!(
            build_from_line("a@b.com", Some("2025-01-02T03:04:05+00:00")),
            "From a@b.com Thu Jan  2 03:04:05 2025\n"
        );
        // Sin dirección se usa el marcador de respaldo.
        assert!(build_from_line("   ", None).starts_with("From unknown@localhost "));
        assert!(parse_message_date("...").is_none());
        assert!(parse_message_date("").is_none());
    }

    #[test]
    fn crlf_normalization_is_idempotent() {
        assert_eq!(normalize_crlf(b"a\nb\r\nc"), b"a\r\nb\r\nc");
        assert_eq!(normalize_crlf(b"a\r\nb\r\n"), b"a\r\nb\r\n");
        assert_eq!(normalize_crlf(b""), b"");
    }
}
