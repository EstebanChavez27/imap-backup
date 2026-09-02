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
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

pub struct Archiver;

impl Archiver {
    /// Comprime un directorio completo en un archivo .zip preservando la estructura interna de carpetas
    pub fn zip_directory(src_dir: &Path, zip_file_path: &Path) -> Result<()> {
        if !src_dir.exists() {
            anyhow::bail!("El directorio fuente a comprimir no existe: {}", src_dir.display());
        }

        if let Some(parent) = zip_file_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = File::create(zip_file_path)
            .with_context(|| format!("No se pudo crear el archivo ZIP: {}", zip_file_path.display()))?;
        let mut zip = ZipWriter::new(file);

        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o755);

        let mut buffer = Vec::new();

        for entry in WalkDir::new(src_dir).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = match path.strip_prefix(src_dir) {
                Ok(p) => p,
                Err(_) => continue,
            };

            let name_str = name.to_string_lossy();
            if name_str.is_empty() {
                continue;
            }

            // Normalizar separadores de ruta para ZIP (siempre '/')
            let zip_entry_name = name_str.replace('\\', "/");

            if path.is_file() {
                // No incluir el propio archivo ZIP si está dentro del mismo directorio
                if path == zip_file_path {
                    continue;
                }
                zip.start_file(&zip_entry_name, options)?;
                let mut f = File::open(path)?;
                buffer.clear();
                f.read_to_end(&mut buffer)?;
                zip.write_all(&buffer)?;
            } else if path.is_dir() {
                if !zip_entry_name.ends_with('/') {
                    zip.add_directory(format!("{}/", zip_entry_name), options)?;
                } else {
                    zip.add_directory(&zip_entry_name, options)?;
                }
            }
        }

        zip.finish()
            .with_context(|| format!("Error finalizando compresión de: {}", zip_file_path.display()))?;
        Ok(())
    }

    /// Elimina de forma segura un directorio y su contenido si se solicita la limpieza
    pub fn cleanup_directory(dir_to_remove: &Path) -> Result<()> {
        if dir_to_remove.exists() && dir_to_remove.is_dir() {
            fs::remove_dir_all(dir_to_remove)
                .with_context(|| format!("Error eliminando directorio temporal: {}", dir_to_remove.display()))?;
        }
        Ok(())
    }

    /// Genera la ruta para un ZIP individual por cuenta
    pub fn get_account_zip_path(base_dir: &Path, domain_label: &str, email: &str) -> PathBuf {
        let safe_domain = sanitize_filename::sanitize(domain_label);
        let safe_email = sanitize_filename::sanitize(email);
        base_dir.join(safe_domain).join(format!("{}_backup.zip", safe_email))
    }

    /// Genera la ruta para un ZIP consolidado global
    pub fn get_consolidated_zip_path(base_dir: &Path) -> PathBuf {
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        base_dir.join(format!("backup_consolidado_{}.zip", timestamp))
    }
}
