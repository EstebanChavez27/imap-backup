// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Formatos de almacenamiento soportados.
//!
//! * `.eml`  — un archivo por mensaje (formato histórico, compatible con la v1).
//! * `.mbox` — un archivo mboxrd por carpeta, importable en Thunderbird/`mb2md`.
//! * Maildir — un directorio por carpeta, con flags en el nombre del archivo.

pub mod maildir;
pub mod mbox;

/// Extrae el valor de una cabecera de un mensaje crudo, sin parsear nada más.
/// Se usa para la línea `From ` de mbox y como respaldo del `Message-ID`.
pub fn header_value(raw: &[u8], header: &str) -> Option<String> {
    const LIMIT: usize = 64 * 1024;
    let head = String::from_utf8_lossy(&raw[..raw.len().min(LIMIT)]);
    for line in head.lines() {
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case(header) {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// Devuelve la dirección contenida en un valor de cabecera `From: Nombre <a@b>`.
pub fn address_of(value: &str) -> String {
    match value.split_once('<') {
        Some((_, rest)) => rest
            .split_once('>')
            .map(|(address, _)| address.trim().to_string())
            .filter(|address| !address.is_empty())
            .unwrap_or_else(|| value.trim().to_string()),
        None => value.trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lookup_is_case_insensitive_and_stops_at_body() {
        let raw = b"From: Uno <uno@ejemplo.com>\r\nSubject: X\r\nMESSAGE-ID: <id@x>\r\n\r\nFrom: falso@ejemplo.com";
        assert_eq!(header_value(raw, "from").unwrap(), "Uno <uno@ejemplo.com>");
        assert_eq!(header_value(raw, "message-id").unwrap(), "<id@x>");
        assert!(header_value(raw, "bcc").is_none());
    }

    #[test]
    fn address_is_extracted_from_display_name() {
        assert_eq!(address_of("Uno <uno@ejemplo.com>"), "uno@ejemplo.com");
        assert_eq!(address_of("solo@ejemplo.com"), "solo@ejemplo.com");
        assert_eq!(address_of("<a@b.com>"), "a@b.com");
    }
}
