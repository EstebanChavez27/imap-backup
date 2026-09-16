// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Compresión ZIP.
//!
//! Diferencias frente a la v1: se copia por bloques (`io::copy`) en lugar de leer
//! cada archivo entero en memoria, se informa progreso, se puede elegir compresión
//! `Store` (los `.eml` con adjuntos ya vienen comprimidos y deflate suele perder
//! tiempo sin ganar tamaño) y el ZIP destino nunca se incluye a sí mismo ni deja
//! archivos `.part` dentro del archivo.

use crate::config::ZipCompression;
use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

#[derive(Debug, Clone, Default)]
pub struct ZipStats {
    pub entries: u64,
    pub files: u64,
    pub bytes: u64,
    pub output_bytes: u64,
    pub skipped: u64,
}

pub fn account_zip_path(base_dir: &Path, label: &str, email: &str) -> PathBuf {
    let account = crate::folders::account_dir(base_dir, label, email);
    let name = crate::folders::sanitize_component(email);
    let name = if name.is_empty() { "cuenta".to_string() } else { name };
    account
        .parent()
        .unwrap_or(base_dir)
        .join(format!("{}_backup.zip", name))
}

pub fn consolidated_zip_path(base_dir: &Path) -> PathBuf {
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    base_dir.join(format!("backup_consolidado_{}.zip", timestamp))
}

/// ¿Este archivo debe quedar fuera del ZIP?
///
/// * nunca el propio ZIP destino;
/// * nunca archivos temporales `.part`;
/// * si el ZIP destino vive dentro del árbol comprimido, tampoco los ZIP previos
///   (si no, cada corrida del consolidado arrastraría los anteriores y crecería).
fn should_skip(path: &Path, destination: &Path, dest_inside_source: bool) -> bool {
    if path == destination {
        return true;
    }
    if path
        .file_name()
        .map(|n| n.to_string_lossy().contains(".part"))
        .unwrap_or(false)
    {
        return true;
    }
    if dest_inside_source {
        if let Some(ext) = path.extension() {
            if ext.eq_ignore_ascii_case("zip") {
                return true;
            }
        }
    }
    false
}

/// Comprime un directorio completo preservando su estructura interna.
///
/// `on_progress` se invoca por cada entrada con `(archivos_procesados, bytes_leídos)`.
pub fn zip_directory(
    src_dir: &Path,
    zip_file_path: &Path,
    compression: ZipCompression,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<ZipStats> {
    if !src_dir.exists() {
        anyhow::bail!(
            "El directorio a comprimir no existe: {}",
            src_dir.display()
        );
    }
    if let Some(parent) = zip_file_path.parent() {
        fs::create_dir_all(parent).ok();
    }

    let tmp_path = crate::fsutil::temp_sibling(zip_file_path);
    let file = File::create(&tmp_path).with_context(|| {
        format!("No se pudo crear el archivo ZIP: {}", tmp_path.display())
    })?;
    let mut zip = ZipWriter::new(file);

    let method = match compression {
        ZipCompression::Deflate => CompressionMethod::Deflated,
        ZipCompression::Store => CompressionMethod::Stored,
    };
    let options = SimpleFileOptions::default()
        .compression_method(method)
        .unix_permissions(0o644);
    let dir_options = SimpleFileOptions::default()
        .compression_method(method)
        .unix_permissions(0o755);

    let mut stats = ZipStats::default();
    let mut buffer = vec![0u8; 256 * 1024];
    let dest_inside_source = zip_file_path.starts_with(src_dir);

    for entry in WalkDir::new(src_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path == src_dir {
            continue;
        }
        if should_skip(path, zip_file_path, dest_inside_source) {
            stats.skipped += 1;
            continue;
        }

        let relative = match path.strip_prefix(src_dir) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        let name = relative.to_string_lossy().replace('\\', "/");
        if name.is_empty() {
            continue;
        }

        if entry.file_type().is_dir() {
            zip.add_directory(format!("{}/", name), dir_options)?;
            stats.entries += 1;
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }

        zip.start_file(&name, options)?;
        let mut source = File::open(path)
            .with_context(|| format!("No se pudo leer para comprimir: {}", path.display()))?;
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            zip.write_all(&buffer[..read])?;
            stats.bytes += read as u64;
        }
        stats.files += 1;
        stats.entries += 1;
        on_progress(stats.files, stats.bytes);
    }

    let file = zip
        .finish()
        .with_context(|| format!("Error finalizando el ZIP: {}", zip_file_path.display()))?;
    drop(file);

    if zip_file_path.exists() {
        let _ = fs::remove_file(zip_file_path);
    }
    fs::rename(&tmp_path, zip_file_path).with_context(|| {
        format!(
            "No se pudo mover {} a {}",
            tmp_path.display(),
            zip_file_path.display()
        )
    })?;
    stats.output_bytes = crate::fsutil::file_len(zip_file_path).unwrap_or(0);
    Ok(stats)
}

#[derive(Debug, Clone)]
pub struct ZipEntryInfo {
    pub index: usize,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

/// Lector ZIP por acceso aleatorio: se indexa una vez (nombre, tamaño) y cada
/// mensaje se descomprime bajo demanda, de modo que un backup de varios GB se
/// restaura con memoria acotada.
pub struct ZipSource {
    path: PathBuf,
    archive: ZipArchive<File>,
    entries: Vec<ZipEntryInfo>,
}

impl ZipSource {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("No se pudo abrir el ZIP: {}", path.display()))?;
        let mut archive = ZipArchive::new(file)
            .with_context(|| "El archivo seleccionado no es un ZIP válido")?;

        let mut entries = Vec::with_capacity(archive.len());
        for index in 0..archive.len() {
            let (name, size, is_dir) = {
                let entry = match archive.by_index_raw(index) {
                    Ok(entry) => entry,
                    Err(_) => continue,
                };
                (
                    entry.name().to_string(),
                    entry.size(),
                    entry.is_dir(),
                )
            };
            entries.push(ZipEntryInfo {
                index,
                name,
                size,
                is_dir,
            });
        }

        Ok(Self {
            path: path.to_path_buf(),
            archive,
            entries,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> &[ZipEntryInfo] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn read_entry(&mut self, index: usize) -> Result<Vec<u8>> {
        let mut entry = self
            .archive
            .by_index(index)
            .with_context(|| format!("Entrada {} ilegible en el ZIP", index))?;
        let mut buffer = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut buffer)
            .with_context(|| format!("Error descomprimiendo '{}'", entry.name()))?;
        Ok(buffer)
    }

    /// Lee `len` bytes a partir de `start` dentro de una entrada. Se usa para sacar
    /// un mensaje puntual de un mbox guardado dentro del ZIP sin descomprimir (ni
    /// mantener en memoria) el archivo completo.
    pub fn read_entry_range(&mut self, index: usize, start: u64, len: u64) -> Result<Vec<u8>> {
        let mut entry = self
            .archive
            .by_index(index)
            .with_context(|| format!("Entrada {} ilegible en el ZIP", index))?;
        let mut pending = start;
        let mut scratch = [0u8; 16 * 1024];
        while pending > 0 {
            let chunk = pending.min(scratch.len() as u64) as usize;
            let read = entry
                .read(&mut scratch[..chunk])
                .with_context(|| format!("Error descomprimiendo '{}'", entry.name()))?;
            if read == 0 {
                return Ok(Vec::new());
            }
            pending -= read as u64;
        }
        let mut buffer = Vec::with_capacity(len.min(4 * 1024 * 1024) as usize);
        (&mut entry)
            .take(len)
            .read_to_end(&mut buffer)
            .with_context(|| format!("Error descomprimiendo '{}'", entry.name()))?;
        Ok(buffer)
    }

    /// Indexa los mensajes de una entrada mbox dentro del ZIP.
    pub fn scan_entry_mbox(&mut self, index: usize) -> Result<Vec<(u64, u64)>> {
        let mut entry = self
            .archive
            .by_index(index)
            .with_context(|| format!("Entrada {} ilegible en el ZIP", index))?;
        let mut reader = BufReader::with_capacity(128 * 1024, &mut entry);
        crate::formats::mbox::scan_offsets_reader(&mut reader)
            .with_context(|| format!("Error leyendo el mbox '{}' del ZIP", entry.name()))
    }
}

/// Cuenta entradas y tamaño total sin descomprimir nada.
pub fn inspect_zip(path: &Path) -> Result<(u64, u64)> {
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
    let mut messages = 0u64;
    let mut bytes = 0u64;
    for index in 0..archive.len() {
        if let Ok(entry) = archive.by_index_raw(index) {
            if !entry.is_dir() {
                messages += 1;
                bytes += entry.size();
            }
        }
    }
    Ok((messages, bytes))
}

/// Copia el contenido de un archivo ZIP a un directorio (usado por la herramienta
/// de línea de comandos `extract`).
pub fn extract_zip(path: &Path, destination: &Path) -> Result<u64> {
    let mut source = ZipSource::open(path)?;
    let mut extracted = 0u64;
    for index in 0..source.len() {
        let info = source.entries()[index].clone();
        if info.is_dir {
            continue;
        }
        let target = destination.join(&info.name);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = source.read_entry(index)?;
        crate::fsutil::write_atomic(&target, &bytes)?;
        extracted += 1;
    }
    Ok(extracted)
}

/// Crea un File en modo lectura/escritura (usado por pruebas y utilidades).
pub fn open_read(path: &Path) -> Result<File> {
    File::open(path).with_context(|| format!("No se pudo abrir {}", path.display()))
}

#[allow(dead_code)]
fn assert_seekable<T: Seek>(_: &T) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "imapb-archive-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_tree(root: &Path) {
        fs::create_dir_all(root.join("INBOX").join("Sub")).unwrap();
        fs::write(root.join("INBOX").join("1_a.eml"), vec![b'a'; 1000]).unwrap();
        fs::write(root.join("INBOX").join("Sub").join("2_b.eml"), vec![b'b'; 500]).unwrap();
        fs::write(root.join(crate::manifest::MANIFEST_FILE), b"{\"version\":2}").unwrap();
    }

    #[test]
    fn zips_tree_preserving_structure_and_reports_progress() {
        let dir = temp_dir("zip");
        let source = dir.join("cuenta");
        sample_tree(&source);
        let zip_path = dir.join("cuenta.zip");

        let mut progress_calls = 0;
        let stats = zip_directory(
            &source,
            &zip_path,
            ZipCompression::Deflate,
            &mut |_, _| progress_calls += 1,
        )
        .unwrap();

        assert_eq!(stats.files, 3);
        assert!(stats.bytes >= 1500);
        assert_eq!(progress_calls, 3);
        assert!(zip_path.exists());

        let mut zip = ZipSource::open(&zip_path).unwrap();
        let names: Vec<String> = zip.entries().iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"INBOX/1_a.eml".to_string()));
        assert!(names.contains(&"INBOX/Sub/2_b.eml".to_string()));
        let index = names.iter().position(|n| n == "INBOX/1_a.eml").unwrap();
        assert_eq!(zip.read_entry(index).unwrap(), vec![b'a'; 1000]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_never_includes_itself_nor_partial_files() {
        let dir = temp_dir("self");
        let source = dir.join("cuenta");
        sample_tree(&source);
        let zip_path = source.join("dentro.zip");
        fs::write(source.join("basura.eml.1234.part"), b"parcial").unwrap();

        zip_directory(&source, &zip_path, ZipCompression::Store, &mut |_, _| {}).unwrap();

        let zip = ZipSource::open(&zip_path).unwrap();
        let names: Vec<String> = zip.entries().iter().map(|e| e.name.clone()).collect();
        assert!(!names.iter().any(|n| n.ends_with(".part")));
        assert!(!names.contains(&"dentro.zip".to_string()));
        assert!(names.contains(&"INBOX/1_a.eml".to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn consolidated_zip_skips_previous_zips_in_the_tree() {
        let dir = temp_dir("consolidated");
        fs::write(dir.join("backup_consolidado_20240101_000000.zip"), b"viejo").unwrap();
        fs::create_dir_all(dir.join("dominio")).unwrap();
        fs::write(dir.join("dominio").join("a.eml"), b"hola").unwrap();

        let out = consolidated_zip_path(&dir);
        zip_directory(&dir, &out, ZipCompression::Deflate, &mut |_, _| {}).unwrap();

        let zip = ZipSource::open(&out).unwrap();
        let names: Vec<String> = zip.entries().iter().map(|e| e.name.clone()).collect();
        assert!(!names.iter().any(|n| n.ends_with(".zip")));
        assert!(names.contains(&"dominio/a.eml".to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_compression_keeps_bytes_and_inspect_counts_entries() {
        let dir = temp_dir("store");
        let source = dir.join("cuenta");
        sample_tree(&source);
        let zip_path = dir.join("store.zip");
        let stats =
            zip_directory(&source, &zip_path, ZipCompression::Store, &mut |_, _| {}).unwrap();
        assert_eq!(stats.output_bytes, zip_path.metadata().unwrap().len());
        let (messages, bytes) = inspect_zip(&zip_path).unwrap();
        assert_eq!(messages, 3);
        assert!(bytes >= 1500);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_restores_files() {
        let dir = temp_dir("extract");
        let source = dir.join("cuenta");
        sample_tree(&source);
        let zip_path = dir.join("x.zip");
        zip_directory(&source, &zip_path, ZipCompression::Deflate, &mut |_, _| {}).unwrap();
        let out = dir.join("out");
        let extracted = extract_zip(&zip_path, &out).unwrap();
        assert_eq!(extracted, 3);
        assert_eq!(fs::read(out.join("INBOX").join("1_a.eml")).unwrap().len(), 1000);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn account_zip_path_lives_next_to_account_dir() {
        let path = account_zip_path(Path::new("/base"), "midominio.com", "a@midominio.com");
        assert!(path.ends_with("midominio.com/a@midominio.com_backup.zip"));
    }
}
