// Copyright (C) 2026 Esteban Chávez / Contributors
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct StorageManager {
    base_dir: PathBuf,
}

impl StorageManager {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Retorna la ruta base para una cuenta específica:
    /// `<base_dir>/<dominio_o_etiqueta>/<cuenta_email>`
    pub fn account_dir(&self, domain_label: &str, email: &str) -> PathBuf {
        let safe_domain = sanitize_filename::sanitize(domain_label);
        let safe_email = sanitize_filename::sanitize(email);
        self.base_dir.join(safe_domain).join(safe_email)
    }

    /// Convierte el nombre de carpeta IMAP (ej. "INBOX/SubFolder" o "[Gmail]/Sent")
    /// en una ruta válida para el sistema de archivos (respetando subcarpetas o sanitizando).
    pub fn folder_path(
        &self,
        domain_label: &str,
        email: &str,
        imap_folder_name: &str,
        delimiter: Option<char>,
    ) -> PathBuf {
        let mut path = self.account_dir(domain_label, email);

        // Separar por el delimitador IMAP si existe (usualmente '/' o '.')
        let parts: Vec<&str> = if let Some(delim) = delimiter {
            imap_folder_name.split(delim).filter(|s| !s.is_empty()).collect()
        } else {
            imap_folder_name.split('/').filter(|s| !s.is_empty()).collect()
        };

        if parts.is_empty() {
            path.push(sanitize_filename::sanitize(imap_folder_name));
        } else {
            for part in parts {
                let sanitized_part = sanitize_filename::sanitize(part);
                if !sanitized_part.is_empty() {
                    path.push(sanitized_part);
                }
            }
        }

        path
    }

    /// Asegura que el directorio exista
    pub fn ensure_dir(&self, dir: &Path) -> Result<()> {
        if !dir.exists() {
            fs::create_dir_all(dir)
                .with_context(|| format!("No se pudo crear el directorio: {}", dir.display()))?;
        }
        Ok(())
    }

    /// Genera el nombre del archivo .eml con formato `<uid>_<safe_message_id>.eml`
    pub fn eml_filename(uid: u32, message_id_header: Option<&str>) -> String {
        let id_part = if let Some(msg_id) = message_id_header {
            let cleaned = msg_id.trim().trim_matches(|c| c == '<' || c == '>');
            let sanitized = sanitize_filename::sanitize(cleaned);
            if sanitized.is_empty() {
                format!("msg_{}", uid)
            } else {
                // Truncar si es excesivamente largo para evitar límites de sistema de archivos
                if sanitized.len() > 60 {
                    sanitized[..60].to_string()
                } else {
                    sanitized
                }
            }
        } else {
            format!("msg_{}", uid)
        };

        format!("{}_{}.eml", uid, id_part)
    }

    /// Comprueba si el archivo ya existe y tiene un tamaño mayor a 0
    pub fn file_exists_and_valid(file_path: &Path, expected_min_bytes: u64) -> bool {
        if let Ok(metadata) = fs::metadata(file_path) {
            metadata.is_file() && metadata.len() >= expected_min_bytes
        } else {
            false
        }
    }

    /// Guarda los bytes del mensaje RFC 822 en disco
    pub fn save_eml(file_path: &Path, content: &[u8]) -> Result<()> {
        let mut file = File::create(file_path)
            .with_context(|| format!("Error creando archivo: {}", file_path.display()))?;
        file.write_all(content)
            .with_context(|| format!("Error escribiendo datos en: {}", file_path.display()))?;
        file.flush()
            .with_context(|| format!("Error sincronizando archivo: {}", file_path.display()))?;
        Ok(())
    }
}
