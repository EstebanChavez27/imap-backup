// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Motor de respaldo, restauración y migración de correo IMAP.
//!
//! La lógica vive en esta biblioteca y **no** depende de la interfaz gráfica: el
//! binario `imap-backup` la usa en modo headless y `imap-backup-gui` la usa desde
//! egui. En la v1 todo estaba entrelazado con `eframe`, así que no era posible
//! automatizar nada.

pub mod archive;
pub mod backup;
pub mod cancel;
pub mod cli;
pub mod config;
pub mod connect;
pub mod events;
pub mod folders;
pub mod formats;
pub mod fsutil;
pub mod hooks;
pub mod manifest;
pub mod migrate;
pub mod report;
pub mod restore;
pub mod sysutil;
pub mod verify;

#[cfg(feature = "gui")]
pub mod ui;

/// Versión empaquetada, disponible también para los binarios.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
