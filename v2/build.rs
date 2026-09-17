// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Incrusta `assets/icon.ico` como recurso de Windows para que los `.exe`
//! muestren el ícono del programa en el Explorer y la barra de tareas.
//!
//! Se detecta el target por la variable `TARGET` (no con `#[cfg(windows)]`)
//! para que también funcione si algún día se cruza-compila a Windows desde
//! otro host. En el resto de las plataformas es un no-op.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows") {
        println!("cargo:rerun-if-changed=assets/icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        // Fallar en voz alta: un build sin ícono es exactamente el bug silencioso
        // que motivó este archivo. Mejor que la release salga roja.
        if let Err(e) = res.compile() {
            panic!("No se pudo compilar el recurso de Windows (ícono): {e}");
        }
    }
}
