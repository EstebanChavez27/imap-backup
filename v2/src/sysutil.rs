// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Acceso a información del sistema operativo que la biblioteca estándar no expone.
//! Por ahora, el espacio libre del volumen destino: permite avisar antes de empezar
//! una descarga que no cabe en disco, en vez de fallar a mitad de camino.

use std::path::Path;

#[cfg(unix)]
pub fn free_space(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs solo escribe en la estructura pasada por puntero y el path es
    // un CString válido y terminado en NUL.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    let block_size = if stat.f_frsize > 0 {
        stat.f_frsize as u64
    } else {
        stat.f_bsize as u64
    };
    Some((stat.f_bavail as u64).saturating_mul(block_size))
}

#[cfg(windows)]
pub fn free_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut available: u64 = 0;
    // SAFETY: se pasa un puntero válido a una cadena UTF-16 terminada en NUL y
    // punteros válidos (o nulos) para los totales.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        None
    } else {
        Some(available)
    }
}

#[cfg(not(any(unix, windows)))]
pub fn free_space(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_space_is_reported_for_temp_dir() {
        let dir = std::env::temp_dir();
        if let Some(bytes) = free_space(&dir) {
            assert!(bytes > 0);
        }
    }
}
