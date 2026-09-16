// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Binario de línea de comandos (`imap-backup`).
//!
//! No usa `windows_subsystem = "windows"` a propósito: en modo headless hace falta
//! ver la salida, los códigos de error y la ayuda.

use imap_backup::cli::{self, Outcome};

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    match cli::dispatch(argv) {
        Outcome::Exit(code) => std::process::exit(code),
        Outcome::LaunchGui => {
            #[cfg(feature = "gui")]
            {
                if let Err(err) = imap_backup::ui::run_native() {
                    eprintln!("No se pudo iniciar la interfaz gráfica: {err}");
                    eprintln!(
                        "Este binario también funciona sin interfaz: 'imap-backup help'."
                    );
                    std::process::exit(cli::EXIT_FATAL);
                }
            }
            #[cfg(not(feature = "gui"))]
            {
                eprintln!(
                    "Esta compilación no incluye la interfaz gráfica (feature 'gui' desactivada).\n\
                     Usá un comando, por ejemplo: imap-backup backup --config config.toml"
                );
                std::process::exit(cli::EXIT_USAGE);
            }
        }
    }
}
