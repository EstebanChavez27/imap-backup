// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Binario exclusivamente gráfico (`imap-backup-gui`).
//!
//! Sin consola en Windows: es el que se distribuye para abrir con doble clic. Todas
//! las operaciones programables están en el binario `imap-backup`, cuya consola sí
//! queda visible para ver el progreso y los códigos de salida.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> eframe::Result<()> {
    imap_backup::ui::run_native()
}
