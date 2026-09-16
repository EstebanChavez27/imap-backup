// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Utilidades de sistema de archivos: escritura atómica, tamaños, permisos privados
//! y espacio libre en disco.

use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Escribe un archivo de forma atómica: primero en un temporal y luego `rename`.
///
/// Esto es lo que evita el problema clásico del backup incremental: un `.eml`
/// truncado por un corte de red o por cerrar la aplicación, que después se
/// considera "ya descargado" y nunca se repara.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("No se pudo crear el directorio: {}", parent.display()))?;
    }
    let tmp = temp_sibling(path);
    {
        let mut file = File::create(&tmp)
            .with_context(|| format!("No se pudo crear el archivo temporal: {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("Error escribiendo datos en: {}", tmp.display()))?;
        file.flush().ok();
        file.sync_all().ok();
    }
    // En Windows rename sobre un destino existente falla: se elimina primero.
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(&tmp, path).with_context(|| {
        format!(
            "No se pudo mover el archivo temporal {} a {}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// Ruta temporal hermana del destino, de modo que el `rename` sea siempre en el
/// mismo sistema de archivos (un `rename` entre volúmenes falla en Windows).
pub fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "archivo".to_string());
    name.push_str(&format!(".{}.part", std::process::id()));
    match path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

pub fn file_len(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| m.len())
}

/// Suma el tamaño de todos los archivos de un directorio (recursivo).
pub fn dir_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

pub fn count_files_with_ext(path: &Path, ext: &str) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case(ext))
                .unwrap_or(false)
        })
        .count() as u64
}

/// Restringe los permisos del archivo al usuario actual (0600) en sistemas Unix.
/// Se usa para el archivo de credenciales y el manifiesto.
pub fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = fs::set_permissions(path, perms);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Espacio libre disponible en el volumen que contiene `path`.
pub fn free_space(path: &Path) -> Option<u64> {
    crate::sysutil::free_space(path)
}

pub fn read_to_vec(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).with_context(|| format!("No se pudo leer: {}", path.display()))
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.2} {}", value, UNITS[unit])
    }
}

pub fn human_duration(secs: f64) -> String {
    if secs < 60.0 {
        format!("{:.1}s", secs)
    } else if secs < 3600.0 {
        format!("{}m {:.0}s", (secs / 60.0) as u64, secs % 60.0)
    } else {
        format!("{}h {:.0}m", (secs / 3600.0) as u64, (secs % 3600.0) / 60.0)
    }
}

/// Estimación de tiempo restante dados los bytes ya procesados.
pub fn eta_seconds(done: u64, total: u64, elapsed_secs: f64) -> Option<f64> {
    if done == 0 || total == 0 || elapsed_secs <= 0.0 || done >= total {
        return None;
    }
    let rate = done as f64 / elapsed_secs;
    if rate <= 0.0 {
        return None;
    }
    Some((total - done) as f64 / rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "imapb-fsutil-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_write_replaces_existing_file_and_leaves_no_partials() {
        let dir = temp_dir("atomic");
        let path = dir.join("sub").join("archivo.eml");
        write_atomic(&path, b"primero").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"primero");
        write_atomic(&path, b"segundo").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"segundo");

        let leftovers = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".part"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_sibling_stays_in_same_directory() {
        let path = PathBuf::from("/tmp/a/b/file.eml");
        let tmp = temp_sibling(&path);
        assert_eq!(tmp.parent(), path.parent());
        assert!(tmp.file_name().unwrap().to_string_lossy().starts_with("file.eml."));
    }

    #[test]
    fn human_bytes_and_duration_are_readable() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.00 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.00 MB");
        assert_eq!(human_duration(45.4), "45.4s");
        assert_eq!(human_duration(125.0), "2m 5s");
        assert_eq!(human_duration(3720.0), "1h 2m");
    }

    #[test]
    fn eta_is_none_when_there_is_no_progress() {
        assert_eq!(eta_seconds(0, 100, 10.0), None);
        assert_eq!(eta_seconds(100, 100, 10.0), None);
        assert_eq!(eta_seconds(50, 100, 10.0), Some(10.0));
    }

    #[test]
    fn dir_size_sums_nested_files() {
        let dir = temp_dir("size");
        fs::write(dir.join("a.eml"), vec![0u8; 100]).unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub").join("b.eml"), vec![0u8; 50]).unwrap();
        assert_eq!(dir_size(&dir), 150);
        assert_eq!(count_files_with_ext(&dir, "eml"), 2);
        let _ = fs::remove_dir_all(&dir);
    }
}
