// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Pestañas de la interfaz: respaldo, restauración, migración, editor de cuentas,
//! consola virtualizada y ventana de reporte.

use super::{App, ProbeState, RunningTask, TabAction};
use crate::config::{AccountConfig, ExportFormat, TlsMode, ZipCompression, ZipMode};
use crate::connect::{ConnOptions, RemoteFolder};
use crate::events::{AccountRunState, LogEntry, LogLevel, Logger, TaskEvent};
use crate::restore::SourceIndex;
use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

// ---------------------------------------------------------------------------
// Estado por pestaña
// ---------------------------------------------------------------------------

pub struct BackupTabState {
    pub output_dir: PathBuf,
    pub concurrency: usize,
    pub zip_mode: ZipMode,
    pub zip_compression: ZipCompression,
    pub format: ExportFormat,
    pub verify: bool,
    pub hash_verify: bool,
    pub cleanup_after_zip: bool,
    pub incremental: bool,
    pub dry_run: bool,
    pub force_full: bool,
    pub show_advanced: bool,
}

impl Default for BackupTabState {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("backups"),
            concurrency: 3,
            zip_mode: ZipMode::PerAccount,
            zip_compression: ZipCompression::Deflate,
            format: ExportFormat::Eml,
            verify: true,
            hash_verify: false,
            cleanup_after_zip: false,
            incremental: true,
            dry_run: false,
            force_full: false,
            show_advanced: false,
        }
    }
}

pub struct RestoreTabState {
    pub source: Option<PathBuf>,
    pub index: Option<SourceIndex>,
    pub scanning: bool,
    pub scan_status: String,
    pub scan_counters: Option<(u64, u64, u64)>,
    pub scan_events: Option<Receiver<TaskEvent>>,
    pub scan_result: Option<Receiver<Result<SourceIndex, String>>>,
    pub scan_cancel: crate::cancel::CancelToken,
    pub dest_host: String,
    pub dest_port: String,
    pub dest_tls: TlsMode,
    pub dest_email: String,
    pub dest_password: String,
    pub prefix: String,
    pub dedup: bool,
    pub dry_run: bool,
    pub concurrency: usize,
    pub show_password: bool,
}

impl Default for RestoreTabState {
    fn default() -> Self {
        Self {
            source: None,
            index: None,
            scanning: false,
            scan_status: "Sin origen seleccionado".to_string(),
            scan_counters: None,
            scan_events: None,
            scan_result: None,
            scan_cancel: crate::cancel::CancelToken::new(),
            dest_host: String::new(),
            dest_port: "993".to_string(),
            dest_tls: TlsMode::Implicit,
            dest_email: String::new(),
            dest_password: String::new(),
            prefix: String::new(),
            dedup: true,
            dry_run: false,
            concurrency: 3,
            show_password: false,
        }
    }
}

impl RestoreTabState {
    /// Construye la conexión de destino desde el formulario.
    pub fn dest_conn(&self) -> Option<ConnOptions> {
        let host = self.dest_host.trim();
        let email = self.dest_email.trim();
        if host.is_empty() || email.is_empty() || self.dest_password.is_empty() {
            return None;
        }
        let port = self.dest_port.trim().parse::<u16>().ok()?;
        Some(ConnOptions {
            host: host.to_string(),
            port,
            tls: self.dest_tls,
            email: email.to_string(),
            password: Some(crate::config::clean_secret(&self.dest_password)),
            oauth_token: None,
            timeout: std::time::Duration::from_secs(45),
            insecure_skip_tls_verify: false,
        })
    }
}

pub struct MigrateTabState {
    pub source_account: Option<String>,
    pub dest_account: Option<String>,
    pub use_config_account: bool,
    pub dest_host: String,
    pub dest_port: String,
    pub dest_tls: TlsMode,
    pub dest_email: String,
    pub dest_password: String,
    pub prefix: String,
    pub dedup: bool,
    pub dry_run: bool,
    pub concurrency: usize,
    pub keep_local: Option<PathBuf>,
    pub keep_local_enabled: bool,
    pub keep_local_format: ExportFormat,
    pub show_password: bool,
}

impl Default for MigrateTabState {
    fn default() -> Self {
        Self {
            source_account: None,
            dest_account: None,
            use_config_account: false,
            dest_host: String::new(),
            dest_port: "993".to_string(),
            dest_tls: TlsMode::Implicit,
            dest_email: String::new(),
            dest_password: String::new(),
            prefix: String::new(),
            dedup: true,
            dry_run: false,
            concurrency: 2,
            keep_local: None,
            keep_local_enabled: false,
            keep_local_format: ExportFormat::Eml,
            show_password: false,
        }
    }
}

impl MigrateTabState {
    /// Copia local efectiva: solo si está activada y tiene ruta.
    pub fn keep_local_path(&self) -> Option<PathBuf> {
        if !self.keep_local_enabled {
            return None;
        }
        self.keep_local
            .clone()
            .filter(|path| !path.as_os_str().is_empty())
    }

    pub fn dest_conn(&self) -> Option<ConnOptions> {
        let host = self.dest_host.trim();
        let email = self.dest_email.trim();
        if host.is_empty() || email.is_empty() || self.dest_password.is_empty() {
            return None;
        }
        let port = self.dest_port.trim().parse::<u16>().ok()?;
        Some(ConnOptions {
            host: host.to_string(),
            port,
            tls: self.dest_tls,
            email: email.to_string(),
            password: Some(crate::config::clean_secret(&self.dest_password)),
            oauth_token: None,
            timeout: std::time::Duration::from_secs(45),
            insecure_skip_tls_verify: false,
        })
    }
}

/// Editor de cuentas (alta/edición) con todos los campos del formato v2.
pub struct AccountModal {
    pub index: Option<usize>,
    pub email: String,
    pub password: String,
    pub host: String,
    pub port: String,
    pub tls: TlsMode,
    pub label: String,
    pub exclude_folders: String,
    pub include_only_folders: String,
    pub password_env: String,
    pub oauth_token_env: String,
    pub insecure_skip_tls_verify: bool,
    pub show_password: bool,
    pub error: Option<String>,
}

impl Default for AccountModal {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountModal {
    pub fn new() -> Self {
        Self {
            index: None,
            email: String::new(),
            password: String::new(),
            host: String::new(),
            port: "993".to_string(),
            tls: TlsMode::Implicit,
            label: String::new(),
            exclude_folders: "Spam, Trash, Papelera".to_string(),
            include_only_folders: String::new(),
            password_env: String::new(),
            oauth_token_env: String::new(),
            insecure_skip_tls_verify: false,
            show_password: false,
            error: None,
        }
    }

    pub fn edit(index: usize, account: &AccountConfig) -> Self {
        Self {
            index: Some(index),
            email: account.email.clone(),
            password: account.password.clone().unwrap_or_default(),
            host: account.host.clone(),
            port: account.port.to_string(),
            tls: account.tls,
            label: account.label.clone().unwrap_or_default(),
            exclude_folders: account.exclude_folders.join(", "),
            include_only_folders: account
                .include_only_folders
                .clone()
                .unwrap_or_default()
                .join(", "),
            password_env: account.password_env.clone().unwrap_or_default(),
            oauth_token_env: account.oauth_token_env.clone().unwrap_or_default(),
            insecure_skip_tls_verify: account.insecure_skip_tls_verify,
            show_password: false,
            error: None,
        }
    }

    pub fn to_account(&self) -> Result<AccountConfig, String> {
        let email = crate::config::clean_secret(self.email.trim());
        let host = crate::config::clean_secret(self.host.trim());
        if email.is_empty() {
            return Err("El correo/usuario no puede estar vacío.".to_string());
        }
        if !email.contains('@') && self.password_env.is_empty() && self.oauth_token_env.is_empty() {
            return Err(
                "El usuario no parece un correo; si es un usuario alternativo, definí password_env."
                    .to_string(),
            );
        }
        if host.is_empty() {
            return Err("El servidor IMAP no puede estar vacío.".to_string());
        }
        let port = self
            .port
            .trim()
            .parse::<u16>()
            .map_err(|_| "El puerto debe ser un número (por ejemplo 993).".to_string())?;
        if port == 0 {
            return Err("El puerto no puede ser 0.".to_string());
        }
        let password = crate::config::clean_secret(&self.password);
        if password.is_empty() && self.password_env.trim().is_empty() && self.oauth_token_env.trim().is_empty() {
            return Err(
                "Indicá una contraseña, o una variable de entorno, o un token OAuth2.".to_string(),
            );
        }

        Ok(AccountConfig {
            email,
            password: if password.is_empty() { None } else { Some(password) },
            password_env: non_empty(&self.password_env),
            oauth_token_env: non_empty(&self.oauth_token_env),
            host,
            port,
            tls: self.tls,
            label: non_empty(&self.label),
            exclude_folders: split_list(&self.exclude_folders),
            include_only_folders: {
                let list = split_list(&self.include_only_folders);
                if list.is_empty() {
                    None
                } else {
                    Some(list)
                }
            },
            insecure_skip_tls_verify: self.insecure_skip_tls_verify,
        })
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Utilidades de pintado
// ---------------------------------------------------------------------------

fn level_color(level: LogLevel) -> Color32 {
    match level {
        LogLevel::Debug => Color32::from_rgb(150, 150, 150),
        LogLevel::Info => Color32::from_rgb(170, 210, 255),
        LogLevel::Success => Color32::from_rgb(140, 230, 150),
        LogLevel::Warning => Color32::from_rgb(255, 215, 0),
        LogLevel::Error => Color32::from_rgb(255, 110, 100),
    }
}

fn state_color(state: &AccountRunState) -> (Color32, String) {
    match state {
        AccountRunState::Queued => (Color32::GRAY, "En cola".to_string()),
        AccountRunState::Running {
            folder,
            current,
            total,
            ..
        } => (
            Color32::from_rgb(255, 215, 0),
            if total > &0 {
                format!("{} ({}/{})", folder, current, total)
            } else if folder.is_empty() {
                "Conectando…".to_string()
            } else {
                folder.clone()
            },
        ),
        AccountRunState::Finished(report) => {
            let text = if report.errors == 0 {
                format!(
                    "OK · {} nuevo(s) · {} omitido(s) · {}",
                    report.new,
                    report.skipped,
                    crate::fsutil::human_bytes(report.bytes)
                )
            } else {
                format!("Con {} error(es)", report.errors)
            };
            let color = if report.errors == 0 && report.verified {
                Color32::from_rgb(140, 230, 150)
            } else if report.errors == 0 {
                Color32::from_rgb(255, 215, 0)
            } else {
                Color32::from_rgb(255, 110, 100)
            };
            (color, text)
        }
        AccountRunState::Failed(message) => (Color32::from_rgb(255, 110, 100), message.clone()),
    }
}

fn progress_fraction(running: Option<&RunningTask>) -> Option<(f64, String)> {
    let task = running?;
    if let Some((messages, folders, bytes)) = task.scan {
        return Some((
            0.0,
            format!(
                "Analizando: {} mensaje(s) en {} carpeta(s) ({})",
                messages,
                folders,
                crate::fsutil::human_bytes(bytes)
            ),
        ));
    }
    let (current, total, folder) = task
        .accounts
        .values()
        .find_map(|state| match state {
            AccountRunState::Running {
                folder,
                current,
                total,
                ..
            } if *total > 0 => Some((*current, *total, folder.clone())),
            _ => None,
        })?;
    Some((
        current as f64 / total as f64,
        format!("{} — {}/{}", folder, current, total),
    ))
}

// ---------------------------------------------------------------------------
// Barra superior contextual
// ---------------------------------------------------------------------------

pub fn top_bar(app: &mut App, ui: &mut egui::Ui, actions: &mut Vec<TabAction>) {
    match app.tab {
        super::Tab::Backup => {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Destino:").strong());
                ui.label(
                    RichText::new(app.backup_state.output_dir.display().to_string())
                        .italics()
                        .color(Color32::LIGHT_GRAY),
                );
                if ui.button("📁 Cambiar carpeta").clicked() {
                    if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                        app.backup_state.output_dir = folder;
                    }
                }
                if ui.button("📂 Abrir destino").clicked() {
                    actions.push(TabAction::OpenFolder(app.backup_state.output_dir.clone()));
                }
                ui.separator();
                ui.label("Cuentas en paralelo:");
                ui.add(egui::Slider::new(&mut app.backup_state.concurrency, 1..=10));
                ui.separator();
                ui.checkbox(
                    &mut app.backup_state.incremental,
                    "Sincronización incremental",
                )
                .on_hover_text("Comparamos UIDVALIDITY/UIDNEXT y tamaños: no se vuelve a bajar lo que ya está completo");
                ui.checkbox(&mut app.backup_state.verify, "Verificar al terminar");
                ui.checkbox(&mut app.backup_state.dry_run, "Simulación");
                ui.checkbox(&mut app.backup_state.force_full, "Forzar descarga completa");
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("ZIP:");
                egui::ComboBox::from_id_source("zip_mode")
                    .selected_text(app.backup_state.zip_mode.label())
                    .show_ui(ui, |ui| {
                        for mode in [ZipMode::PerAccount, ZipMode::Consolidated, ZipMode::None] {
                            ui.selectable_value(&mut app.backup_state.zip_mode, mode, mode.label());
                        }
                    });
                egui::ComboBox::from_id_source("zip_compression")
                    .selected_text(app.backup_state.zip_compression.label())
                    .show_ui(ui, |ui| {
                        for mode in [ZipCompression::Deflate, ZipCompression::Store] {
                            ui.selectable_value(
                                &mut app.backup_state.zip_compression,
                                mode,
                                mode.label(),
                            );
                        }
                    });
                ui.separator();
                ui.label("Formato de salida:");
                egui::ComboBox::from_id_source("export_format")
                    .selected_text(app.backup_state.format.label())
                    .show_ui(ui, |ui| {
                        for format in [ExportFormat::Eml, ExportFormat::Mbox, ExportFormat::Maildir] {
                            ui.selectable_value(&mut app.backup_state.format, format, format.label());
                        }
                    });
                ui.separator();
                ui.checkbox(&mut app.backup_state.hash_verify, "Calcular SHA-256")
                    .on_hover_text("Más lento, pero permite verificar la integridad local después");
                ui.checkbox(
                    &mut app.backup_state.cleanup_after_zip,
                    "Borrar originales tras ZIP",
                );
            });
        }
        super::Tab::Restore => {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(
                        "Sube los mensajes directamente al buzón por APPEND, sin límites de webmail.",
                    )
                    .color(Color32::LIGHT_BLUE),
                );
                ui.separator();
                ui.checkbox(&mut app.restore_state.dedup, "Omitir los que ya existen")
                    .on_hover_text("Compara Message-ID en el destino: la restauración se puede repetir sin duplicar");
                ui.checkbox(&mut app.restore_state.dry_run, "Simulación");
                ui.separator();
                ui.label("Carpetas en paralelo:");
                ui.add(egui::Slider::new(&mut app.restore_state.concurrency, 1..=10));
            });
        }
        super::Tab::Migrate => {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(
                        "Copia los mensajes de un IMAP a otro sin pasar por disco (ni ZIP ni .eml).",
                    )
                    .color(Color32::LIGHT_BLUE),
                );
                ui.separator();
                ui.checkbox(&mut app.migrate_state.dedup, "Omitir los que ya existen");
                ui.checkbox(&mut app.migrate_state.dry_run, "Simulación");
                ui.separator();
                ui.label("En paralelo:");
                ui.add(egui::Slider::new(&mut app.migrate_state.concurrency, 1..=10));
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Barra inferior: acción, progreso y consola
// ---------------------------------------------------------------------------

pub fn bottom_bar(app: &mut App, ui: &mut egui::Ui, actions: &mut Vec<TabAction>) {
    let busy = app
        .running
        .as_ref()
        .map(|task| !task.is_finished())
        .unwrap_or(false);

    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        let (label, action, enabled) = match app.tab {
            super::Tab::Backup => (
                if busy {
                    "⏳ Respaldando…"
                } else if app.backup_state.dry_run {
                    "🔍 Analizar (simulación)"
                } else {
                    "🚀 Iniciar respaldo"
                },
                TabAction::StartBackup,
                !app.config.accounts.is_empty(),
            ),
            super::Tab::Restore => (
                if busy {
                    "⏳ Subiendo…"
                } else if app.restore_state.dry_run {
                    "🔍 Analizar (simulación)"
                } else {
                    "🚀 Iniciar restauración"
                },
                TabAction::StartRestore,
                app.restore_state.index.is_some(),
            ),
            super::Tab::Migrate => (
                if busy {
                    "⏳ Migrando…"
                } else {
                    "🚀 Iniciar migración"
                },
                TabAction::StartMigrate,
                app.migrate_state.source_account.is_some(),
            ),
        };

        let button = egui::Button::new(RichText::new(label).size(15.0).strong()).fill(
            if busy || !enabled {
                Color32::from_rgb(70, 70, 70)
            } else {
                Color32::from_rgb(34, 120, 60)
            },
        );
        if ui.add_enabled(!busy && enabled, button).clicked() {
            actions.push(action);
        }

        if busy
            && ui
                .button(RichText::new("⏹ Cancelar").color(Color32::from_rgb(255, 150, 140)))
                .clicked()
            {
                actions.push(TabAction::Cancel);
            }

        if let Some((fraction, text)) = progress_fraction(app.running.as_ref()) {
            ui.add(
                egui::ProgressBar::new(fraction as f32)
                    .desired_width(320.0)
                    .show_percentage()
                    .text(text),
            );
        } else if let Some(report) = &app.last_report {
            let color = if report.has_errors() {
                Color32::from_rgb(255, 215, 0)
            } else {
                Color32::from_rgb(140, 230, 150)
            };
            ui.label(RichText::new(report.summary_line()).color(color).strong());
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if app.last_report.is_some() && ui.button("📊 Ver reporte").clicked() {
                app.show_report = true;
            }
            if ui.button("🧾 Exportar logs").clicked() {
                actions.push(TabAction::ExportLogs);
            }
            if let Some(path) = &app.config_path {
                if ui.button("📄 Abrir carpeta del config").clicked() {
                    if let Some(parent) = path.parent() {
                        actions.push(TabAction::OpenFolder(parent.to_path_buf()));
                    }
                }
            }
        });
    });

    ui.add_space(4.0);
    ui.separator();
    console_ui(app, ui);
}

fn console_ui(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("📟 Consola").strong());
        ui.add(
            egui::TextEdit::singleline(&mut app.log_filter)
                .hint_text("Filtrar…")
                .desired_width(180.0),
        );
        egui::ComboBox::from_id_source("log_level")
            .selected_text(format!("Desde {}", app.log_min_level.as_str()))
            .show_ui(ui, |ui| {
                for level in [
                    LogLevel::Debug,
                    LogLevel::Info,
                    LogLevel::Success,
                    LogLevel::Warning,
                    LogLevel::Error,
                ] {
                    ui.selectable_value(&mut app.log_min_level, level, level.as_str());
                }
            });
        ui.checkbox(&mut app.auto_scroll, "Auto-scroll");
        ui.label(
            RichText::new(format!(
                "{} línea(s) (buffer {})",
                app.logger.total(),
                app.logger.capacity()
            ))
            .color(Color32::GRAY)
            .small(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("🗑 Limpiar").clicked() {
                app.log_lines.clear();
                app.logger.clear();
                app.log_cursor = app.logger.total();
            }
        });
    });

    const MAX_VISIBLE_ROWS: usize = 1200;
    let filter = app.log_filter.trim().to_lowercase();
    let minimum = app.log_min_level;
    let filtered: Vec<&LogEntry> = app
        .log_lines
        .iter()
        .filter(|entry| entry.level >= minimum)
        .filter(|entry| {
            filter.is_empty()
                || entry.message.to_lowercase().contains(&filter)
                || entry.scope.to_lowercase().contains(&filter)
        })
        .collect();

    let hidden = filtered.len().saturating_sub(MAX_VISIBLE_ROWS);
    let rows: Vec<&LogEntry> = filtered
        .into_iter()
        .skip(hidden)
        .collect();

    egui::ScrollArea::vertical()
        .id_source("log_scroll")
        .auto_shrink([false, false])
        .stick_to_bottom(app.auto_scroll)
        .show(ui, |ui| {
            if hidden > 0 {
                ui.label(
                    RichText::new(format!(
                        "… {} línea(s) anteriores ocultas (usá el filtro o exportá los logs para verlas)",
                        hidden
                    ))
                    .small()
                    .color(Color32::GRAY),
                );
            }
            // Se dibuja un máximo acotado de líneas: un backup de 100.000 mensajes no
            // degrada el repintado (la v1 volcaba el vector completo cada frame).
            ui.style_mut().spacing.item_spacing.y = 1.0;
            for entry in rows {
                let color = level_color(entry.level);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&entry.ts).monospace().small().color(Color32::DARK_GRAY));
                    ui.label(RichText::new(entry.level.badge()).monospace().small().color(color));
                    if !entry.scope.is_empty() {
                        ui.label(
                            RichText::new(crate::folders::truncate_chars(&entry.scope, 22))
                                .monospace()
                                .small()
                                .color(Color32::from_rgb(150, 180, 210)),
                        );
                    }
                    ui.label(RichText::new(&entry.message).monospace().small().color(color));
                });
            }
        });
}

// ---------------------------------------------------------------------------
// Pestaña de respaldo
// ---------------------------------------------------------------------------

pub fn backup_tab(app: &mut App, ui: &mut egui::Ui, actions: &mut Vec<TabAction>) {
    ui.horizontal(|ui| {
        ui.heading(format!("Buzones a respaldar ({})", app.config.accounts.len()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button(RichText::new("➕ Añadir cuenta").strong().color(Color32::from_rgb(140, 230, 150)))
                .clicked()
            {
                app.account_modal = Some(AccountModal::new());
            }
        });
    });
    ui.label(
        RichText::new(
            "Cada cuenta se baja en modo solo lectura (EXAMINE) con una única sesión IMAP reutilizada.",
        )
        .color(Color32::GRAY)
        .small(),
    );
    ui.add_space(6.0);

    if app.config.accounts.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.label(
                RichText::new(
                    "No hay cuentas configuradas.\nUsá '➕ Añadir cuenta' o cargá un config.toml/config.json.",
                )
                .italics()
                .color(Color32::GRAY),
            );
        });
        return;
    }

    let mut to_edit: Option<usize> = None;
    let mut to_delete: Option<usize> = None;
    let mut to_probe: Option<String> = None;

    let accounts = &app.config.accounts;
    let states = &app
        .running
        .as_ref()
        .map(|task| task.accounts.clone())
        .unwrap_or_default();

    TableBuilder::new(ui)
        .striped(true)
        .column(Column::initial(240.0).at_least(180.0))
        .column(Column::initial(190.0).at_least(140.0))
        .column(Column::initial(110.0).at_least(80.0))
        .column(Column::remainder())
        .column(Column::exact(200.0))
        .header(22.0, |mut header| {
            for title in ["Cuenta", "Servidor", "Seguridad", "Estado", "Acciones"] {
                header.col(|ui| {
                    ui.label(RichText::new(title).strong());
                });
            }
        })
        .body(|body| {
            body.rows(30.0, accounts.len(), |mut row| {
                let index = row.index();
                let account = &accounts[index];

                row.col(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&account.email).strong());
                        if let Some(label) = &account.label {
                            ui.label(RichText::new(label).small().color(Color32::LIGHT_BLUE));
                        }
                    });
                });
                row.col(|ui| {
                    ui.label(format!("{}:{}", account.host, account.effective_port()));
                });
                row.col(|ui| {
                    let (text, color) = match account.tls {
                        TlsMode::Implicit => ("TLS", Color32::from_rgb(140, 230, 150)),
                        TlsMode::StartTls => ("STARTTLS", Color32::from_rgb(200, 220, 140)),
                        TlsMode::Plain => ("SIN CIFRADO", Color32::from_rgb(255, 110, 100)),
                    };
                    ui.label(RichText::new(text).color(color).small());
                });
                row.col(|ui| {
                    match states.get(&account.email) {
                        Some(state) => {
                            let (color, text) = state_color(state);
                            ui.label(RichText::new(text).color(color).small());
                        }
                        None => {
                            let filters = account.exclude_folders.len()
                                + account
                                    .include_only_folders
                                    .as_ref()
                                    .map(|list| list.len())
                                    .unwrap_or(0);
                            if filters > 0 {
                                ui.label(
                                    RichText::new(format!("{} regla(s) de carpetas", filters))
                                        .small()
                                        .color(Color32::GRAY),
                                );
                            }
                        }
                    }
                });
                row.col(|ui| {
                    if ui.small_button("🔌 Probar").clicked() {
                        to_probe = Some(account.email.clone());
                    }
                    if ui.small_button("✏ Editar").clicked() {
                        to_edit = Some(index);
                    }
                    if ui.small_button("🗑").on_hover_text("Eliminar").clicked() {
                        to_delete = Some(index);
                    }
                });
            });
        });

    if let Some(email) = to_probe {
        actions.push(TabAction::Probe(email));
    }
    if let Some(index) = to_edit {
        let account = app.config.accounts[index].clone();
        app.account_modal = Some(AccountModal::edit(index, &account));
    }
    if let Some(index) = to_delete {
        let email = app.config.accounts[index].email.clone();
        app.config.accounts.remove(index);
        app.secrets.remove(&email);
        app.notify(format!("Cuenta '{}' eliminada.", email), true);
    }

    ui.add_space(10.0);
    folder_filter_ui(app, ui, actions);
}

/// Lista de carpetas del último sondeo, con casillas para armar las reglas de
/// inclusión/exclusión sin escribir nombres a mano.
fn folder_filter_ui(app: &mut App, ui: &mut egui::Ui, actions: &mut Vec<TabAction>) {
    let (account_email, folders): (String, Vec<RemoteFolder>) = match &app.probe {
        ProbeState::Loaded { account, folders } => (account.clone(), folders.clone()),
        ProbeState::Running { account } => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.label(format!("Consultando carpetas de {}…", account));
            });
            return;
        }
        ProbeState::Error { account, message } => {
            ui.label(
                RichText::new(format!("No se pudo listar {}: {}", account, message))
                    .color(Color32::from_rgb(255, 110, 100)),
            );
            return;
        }
        ProbeState::Idle => return,
    };

    let Some(index) = app
        .config
        .accounts
        .iter()
        .position(|account| account.email == account_email)
    else {
        return;
    };

    ui.group(|ui| {
        ui.label(
            RichText::new(format!("Carpetas de {} (marcá las que querés respaldar)", account_email))
                .strong(),
        );
        ui.label(
            RichText::new(
                "Las carpetas virtuales \\All se omiten siempre. Las no seleccionables no se pueden abrir.",
            )
            .small()
            .color(Color32::GRAY),
        );
        ui.add_space(4.0);

        let excluded = app.config.accounts[index].exclude_folders.clone();
        let mut new_excluded = excluded.clone();
        let mut changed = false;

        egui::ScrollArea::vertical()
            .max_height(150.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for folder in &folders {
                    let excluded_now = excluded.iter().any(|rule| {
                        rule.eq_ignore_ascii_case(&folder.name)
                            || folder
                                .name
                                .to_lowercase()
                                .ends_with(&format!("/{}", rule.to_lowercase()))
                    });
                    let mut include = !excluded_now;
                    let label = format!(
                        "{}{}",
                        folder.name,
                        folder
                            .special_use
                            .map(|use_kind| format!("  [{}]", use_kind.label()))
                            .unwrap_or_default()
                    );
                    let response = ui.checkbox(&mut include, label);
                    if folder.selectable {
                        if response.changed() {
                            changed = true;
                            if include {
                                new_excluded.retain(|rule| !rule.eq_ignore_ascii_case(&folder.name));
                            } else if !new_excluded
                                .iter()
                                .any(|rule| rule.eq_ignore_ascii_case(&folder.name))
                            {
                                new_excluded.push(folder.name.clone());
                            }
                        }
                    } else {
                        response.on_hover_text("Carpeta \\Noselect: no se puede abrir");
                    }
                }
            });

        if changed {
            app.config.accounts[index].exclude_folders = new_excluded;
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("✅ Marcar todas").clicked() {
                app.config.accounts[index].exclude_folders.clear();
                app.config.accounts[index].include_only_folders = None;
            }
            if ui.button("🚫 Desmarcar todas").clicked() {
                app.config.accounts[index].exclude_folders = folders
                    .iter()
                    .filter(|folder| folder.selectable && !folder.name.eq_ignore_ascii_case("INBOX"))
                    .map(|folder| folder.name.clone())
                    .collect();
            }
            if ui
                .button("🎯 Solo las marcadas")
                .on_hover_text("Convierte la selección actual en include_only_folders")
                .clicked()
            {
                let selected: Vec<String> = folders
                    .iter()
                    .filter(|folder| {
                        !app.config.accounts[index]
                            .exclude_folders
                            .iter()
                            .any(|rule| rule.eq_ignore_ascii_case(&folder.name))
                    })
                    .map(|folder| folder.name.clone())
                    .collect();
                app.config.accounts[index].include_only_folders = Some(selected);
                app.config.accounts[index].exclude_folders.clear();
                app.notify("Se respaldarán solo las carpetas marcadas.", true);
            }
            if ui.button("🔌 Volver a probar").clicked() {
                actions.push(TabAction::Probe(account_email.clone()));
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Pestaña de restauración
// ---------------------------------------------------------------------------

pub fn restore_tab(app: &mut App, ui: &mut egui::Ui, actions: &mut Vec<TabAction>) {
    ui.heading("Restaurar un backup dentro de un buzón");
    ui.label(
        RichText::new(
            "Acepta ZIP, carpeta con .eml, archivos .mbox y Maildir. Se leen en streaming: no hace falta memoria para todo el backup.",
        )
        .color(Color32::GRAY)
        .small(),
    );
    ui.add_space(8.0);

    ui.group(|ui| {
        ui.label(RichText::new("1. Destino").strong());
        egui::Grid::new("restore_dest")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("Servidor IMAP:");
                ui.text_edit_singleline(&mut app.restore_state.dest_host);
                ui.end_row();

                ui.label("Puerto:");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut app.restore_state.dest_port).desired_width(70.0));
                    egui::ComboBox::from_id_source("restore_tls")
                        .selected_text(app.restore_state.dest_tls.label())
                        .show_ui(ui, |ui| {
                            for mode in [TlsMode::Implicit, TlsMode::StartTls, TlsMode::Plain] {
                                ui.selectable_value(&mut app.restore_state.dest_tls, mode, mode.label());
                            }
                        });
                });
                ui.end_row();

                ui.label("Correo destino:");
                ui.text_edit_singleline(&mut app.restore_state.dest_email);
                ui.end_row();

                ui.label("Contraseña:");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut app.restore_state.dest_password)
                            .password(!app.restore_state.show_password)
                            .desired_width(220.0),
                    );
                    ui.checkbox(&mut app.restore_state.show_password, "Ver");
                });
                ui.end_row();

                ui.label("Restaurar dentro de:");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut app.restore_state.prefix)
                            .hint_text("(vacío = mismas carpetas)")
                            .desired_width(220.0),
                    );
                    ui.label(
                        RichText::new("útil para probar sin mezclar con lo existente")
                            .small()
                            .color(Color32::GRAY),
                    );
                });
                ui.end_row();
            });
    });

    ui.add_space(8.0);

    ui.group(|ui| {
        ui.label(RichText::new("2. Origen del backup").strong());
        ui.horizontal_wrapped(|ui| {
            if ui.button("📦 Elegir archivo .ZIP").clicked() {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter("Backup ZIP", &["zip"])
                    .pick_file()
                {
                    app.restore_state.source = Some(file.clone());
                    actions.push(TabAction::ScanSource(file));
                }
            }
            if ui.button("📁 Elegir carpeta de correos").clicked() {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    app.restore_state.source = Some(folder.clone());
                    actions.push(TabAction::ScanSource(folder));
                }
            }
            if ui.button("📄 Elegir archivo .mbox").clicked() {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter("mbox", &["mbox"])
                    .pick_file()
                {
                    app.restore_state.source = Some(file.clone());
                    actions.push(TabAction::ScanSource(file));
                }
            }
            if let Some(source) = app.restore_state.source.clone() {
                if ui.button("🔄 Reanalizar").clicked() {
                    actions.push(TabAction::ScanSource(source));
                }
            }
        });

        ui.add_space(6.0);
        if app.restore_state.scanning {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                let text = match app.restore_state.scan_counters {
                    Some((messages, folders, bytes)) => format!(
                        "Analizando… {} mensaje(s), {} carpeta(s), {}",
                        messages,
                        folders,
                        crate::fsutil::human_bytes(bytes)
                    ),
                    None => app.restore_state.scan_status.clone(),
                };
                ui.label(text);
            });
        } else if let Some(source) = &app.restore_state.source {
            ui.label(
                RichText::new(format!("Origen: {}", source.display()))
                    .color(Color32::LIGHT_GREEN),
            );
        }

        if let Some(index) = &app.restore_state.index {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("✔ {}", index.summary()))
                    .color(Color32::from_rgb(120, 210, 255))
                    .strong(),
            );
            ui.label(
                RichText::new(format!(
                    "Manifiesto: {}",
                    if index.manifest.is_some() {
                        "detectado (se restauran fechas y flags reales)"
                    } else {
                        "no encontrado (origen antiguo: se usa el formato del archivo)"
                    }
                ))
                .small()
                .color(Color32::GRAY),
            );

            egui::ScrollArea::vertical()
                .max_height(160.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for folder in &index.folders {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&folder.remote_name).strong().monospace());
                            if let Some(use_kind) = folder.special_use {
                                ui.label(
                                    RichText::new(format!("[{}]", use_kind.label()))
                                        .small()
                                        .color(Color32::LIGHT_BLUE),
                                );
                            }
                            ui.label(
                                RichText::new(format!(
                                    "{} mensaje(s) · {}",
                                    folder.items.len(),
                                    crate::fsutil::human_bytes(folder.total_bytes())
                                ))
                                .small()
                                .color(Color32::GRAY),
                            );
                        });
                    }
                });
        } else if !app.restore_state.scanning && app.restore_state.source.is_none() {
            ui.label(
                RichText::new("Ningún origen seleccionado.")
                    .italics()
                    .color(Color32::GRAY),
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Pestaña de migración
// ---------------------------------------------------------------------------

pub fn migrate_tab(app: &mut App, ui: &mut egui::Ui) {
    ui.heading("Migrar correo de un servidor IMAP a otro");
    ui.label(
        RichText::new(
            "Lee del origen y escribe en el destino por streaming, sin ZIP ni archivos temporales.",
        )
        .color(Color32::GRAY)
        .small(),
    );
    ui.add_space(8.0);

    ui.group(|ui| {
        ui.label(RichText::new("Origen").strong());
        let accounts: Vec<String> = app
            .config
            .accounts
            .iter()
            .map(|account| account.email.clone())
            .collect();
        if accounts.is_empty() {
            ui.label(
                RichText::new("Agregá la cuenta origen en la pestaña de respaldo.")
                    .color(Color32::from_rgb(255, 215, 0)),
            );
            return;
        }

        egui::ComboBox::from_id_source("migrate_source")
            .selected_text(
                app.migrate_state
                    .source_account
                    .clone()
                    .unwrap_or_else(|| "Elegir cuenta…".to_string()),
            )
            .show_ui(ui, |ui| {
                for email in &accounts {
                    ui.selectable_value(
                        &mut app.migrate_state.source_account,
                        Some(email.clone()),
                        email,
                    );
                }
            });
        ui.label(
            RichText::new(
                "Usa la configuración de esa cuenta (contraseña, exclusiones e include_only_folders).",
            )
            .small()
            .color(Color32::GRAY),
        );
    });

    ui.add_space(8.0);

    ui.group(|ui| {
        ui.label(RichText::new("Destino").strong());
        ui.checkbox(
            &mut app.migrate_state.use_config_account,
            "Usar una cuenta ya configurada",
        );

        if app.migrate_state.use_config_account {
            let accounts: Vec<String> = app
                .config
                .accounts
                .iter()
                .map(|account| account.email.clone())
                .collect();
            egui::ComboBox::from_id_source("migrate_dest_account")
                .selected_text(
                    app.migrate_state
                        .dest_account
                        .clone()
                        .unwrap_or_else(|| "Elegir cuenta…".to_string()),
                )
                .show_ui(ui, |ui| {
                    for email in &accounts {
                        if ui
                            .selectable_value(
                                &mut app.migrate_state.dest_account,
                                Some(email.clone()),
                                email,
                            )
                            .clicked()
                        {
                            if let Some(account) = app.config.account_by_email(email).cloned() {
                                app.migrate_state.dest_host = account.host.clone();
                                app.migrate_state.dest_port = account.effective_port().to_string();
                                app.migrate_state.dest_tls = account.tls;
                                app.migrate_state.dest_email = account.email.clone();
                                if let Ok(password) = account.resolve_password(&app.secrets, false)
                                {
                                    app.migrate_state.dest_password = password;
                                }
                            }
                        }
                    }
                });
        } else {
            egui::Grid::new("migrate_dest")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Servidor IMAP:");
                    ui.text_edit_singleline(&mut app.migrate_state.dest_host);
                    ui.end_row();

                    ui.label("Puerto:");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut app.migrate_state.dest_port)
                                .desired_width(70.0),
                        );
                        egui::ComboBox::from_id_source("migrate_tls")
                            .selected_text(app.migrate_state.dest_tls.label())
                            .show_ui(ui, |ui| {
                                for mode in [TlsMode::Implicit, TlsMode::StartTls, TlsMode::Plain] {
                                    ui.selectable_value(
                                        &mut app.migrate_state.dest_tls,
                                        mode,
                                        mode.label(),
                                    );
                                }
                            });
                    });
                    ui.end_row();

                    ui.label("Correo destino:");
                    ui.text_edit_singleline(&mut app.migrate_state.dest_email);
                    ui.end_row();

                    ui.label("Contraseña:");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut app.migrate_state.dest_password)
                                .password(!app.migrate_state.show_password)
                                .desired_width(220.0),
                        );
                        ui.checkbox(&mut app.migrate_state.show_password, "Ver");
                    });
                    ui.end_row();
                });
        }
    });

    ui.add_space(8.0);

    ui.group(|ui| {
        ui.label(RichText::new("Opciones").strong());
        ui.horizontal(|ui| {
            ui.label("Migrar dentro de:");
            ui.add(
                egui::TextEdit::singleline(&mut app.migrate_state.prefix)
                    .hint_text("(vacío = mismas carpetas)")
                    .desired_width(220.0),
            );
        });
        ui.horizontal(|ui| {
            ui.checkbox(
                &mut app.migrate_state.keep_local_enabled,
                "Dejar además una copia local",
            );
            if ui.button("📁 Elegir carpeta de copia").clicked() {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    app.migrate_state.keep_local = Some(folder);
                    app.migrate_state.keep_local_enabled = true;
                }
            }
            if let Some(path) = &app.migrate_state.keep_local {
                ui.label(RichText::new(path.display().to_string()).small().color(Color32::LIGHT_GREEN));
            }
            egui::ComboBox::from_id_source("keep_local_format")
                .selected_text(app.migrate_state.keep_local_format.label())
                .show_ui(ui, |ui| {
                    for format in [ExportFormat::Eml, ExportFormat::Maildir, ExportFormat::Mbox] {
                        ui.selectable_value(
                            &mut app.migrate_state.keep_local_format,
                            format,
                            format.label(),
                        );
                    }
                });
        });
        ui.label(
            RichText::new(
                "La copia local es una exportación simple (sin manifiesto); la reanudación se apoya en el Message-ID del destino.",
            )
            .small()
            .color(Color32::GRAY),
        );
    });
}

// ---------------------------------------------------------------------------
// Modal de cuentas
// ---------------------------------------------------------------------------

pub fn account_modal(app: &mut App, ctx: &egui::Context) {
    let Some(mut modal) = app.account_modal.take() else {
        return;
    };
    let mut open = true;
    let mut save = false;
    let mut cancel = false;

    egui::Window::new(if modal.index.is_some() {
        "Editar cuenta IMAP"
    } else {
        "Añadir cuenta IMAP"
    })
    .collapsible(false)
    .resizable(false)
    .open(&mut open)
    .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
    .show(ctx, |ui| {
        ui.set_min_width(460.0);
        if let Some(error) = &modal.error {
            ui.label(RichText::new(error).color(Color32::from_rgb(255, 110, 100)).strong());
            ui.add_space(4.0);
        }

        egui::Grid::new("account_edit")
            .num_columns(2)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                ui.label("Correo / Usuario:");
                ui.text_edit_singleline(&mut modal.email);
                ui.end_row();

                ui.label("Contraseña:");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut modal.password)
                            .password(!modal.show_password)
                            .desired_width(220.0),
                    );
                    ui.checkbox(&mut modal.show_password, "Ver");
                });
                ui.end_row();

                ui.label("Variable de entorno:");
                ui.add(
                    egui::TextEdit::singleline(&mut modal.password_env)
                        .hint_text("IMAP_PASSWORD (recomendado para automatizar)")
                        .desired_width(260.0),
                );
                ui.end_row();

                ui.label("Token OAuth2 (env):");
                ui.add(
                    egui::TextEdit::singleline(&mut modal.oauth_token_env)
                        .hint_text("para Microsoft 365 / Google Workspace")
                        .desired_width(260.0),
                );
                ui.end_row();

                ui.label("Servidor IMAP:");
                ui.text_edit_singleline(&mut modal.host);
                ui.end_row();

                ui.label("Puerto:");
                ui.add(egui::TextEdit::singleline(&mut modal.port).desired_width(80.0));
                ui.end_row();

                ui.label("Seguridad:");
                egui::ComboBox::from_id_source("account_tls")
                    .selected_text(modal.tls.label())
                    .show_ui(ui, |ui| {
                        for mode in [TlsMode::Implicit, TlsMode::StartTls, TlsMode::Plain] {
                            ui.selectable_value(&mut modal.tls, mode, mode.label());
                        }
                    });
                ui.end_row();

                ui.label("Etiqueta / dominio:");
                ui.add(
                    egui::TextEdit::singleline(&mut modal.label)
                        .hint_text("se usa como carpeta raíz del backup")
                        .desired_width(260.0),
                );
                ui.end_row();

                ui.label("Excluir carpetas:");
                ui.add(
                    egui::TextEdit::singleline(&mut modal.exclude_folders)
                        .hint_text("Spam, Trash, Papelera")
                        .desired_width(300.0),
                );
                ui.end_row();

                ui.label("Solo estas carpetas:");
                ui.add(
                    egui::TextEdit::singleline(&mut modal.include_only_folders)
                        .hint_text("INBOX, Enviados (opcional)")
                        .desired_width(300.0),
                );
                ui.end_row();

                ui.label("Certificado:");
                ui.checkbox(
                    &mut modal.insecure_skip_tls_verify,
                    "Aceptar certificados inválidos (solo servidores propios)",
                );
                ui.end_row();
            });

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button(RichText::new("Guardar").strong()).clicked() {
                save = true;
            }
            if ui.button("Cancelar").clicked() {
                cancel = true;
            }
        });
    });

    if save {
        match modal.to_account() {
            Ok(account) => {
                match modal.index {
                    Some(index) if index < app.config.accounts.len() => {
                        app.config.accounts[index] = account;
                        app.notify("Cuenta actualizada.", true);
                    }
                    _ => {
                        app.config.accounts.push(account);
                        app.notify("Cuenta añadida.", true);
                    }
                }
                app.account_modal = None;
                return;
            }
            Err(error) => {
                modal.error = Some(error);
            }
        }
    }

    if cancel || !open {
        app.account_modal = None;
    } else {
        app.account_modal = Some(modal);
    }
}

// ---------------------------------------------------------------------------
// Ventana de reporte
// ---------------------------------------------------------------------------

pub fn report_window(app: &mut App, ctx: &egui::Context) {
    let Some(report) = app.last_report.clone() else {
        app.show_report = false;
        return;
    };

    let mut open = app.show_report;
    egui::Window::new("Reporte de la última operación")
        .collapsible(false)
        .resizable(true)
        .default_size([720.0, 460.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(RichText::new(report.summary_line()).strong());
            if let Some(estimated) = report.estimated_bytes {
                ui.label(
                    RichText::new(format!(
                        "Volumen procesado: {} · Libre en destino al iniciar: {}",
                        crate::fsutil::human_bytes(estimated),
                        report
                            .free_space_before
                            .map(crate::fsutil::human_bytes)
                            .unwrap_or_else(|| "desconocido".to_string())
                    ))
                    .small()
                    .color(Color32::GRAY),
                );
            }
            ui.add_space(6.0);

            let mut export = false;
            ui.horizontal(|ui| {
                if ui.button("💾 Guardar reporte (JSON + CSV)").clicked() {
                    export = true;
                }
                if ui.button("📂 Abrir carpeta de destino").clicked() {
                    let path = app.backup_state.output_dir.clone();
                    app.open_in_file_manager(&path);
                }
                ui.label(
                    RichText::new("Los reportes quedan también en el log si usás --report en la CLI.")
                        .small()
                        .color(Color32::GRAY),
                );
            });
            if export {
                if let Some(path) = rfd::FileDialog::new()
                    .set_file_name("reporte-imap-backup.json")
                    .save_file()
                {
                    match report.write_reports(&path) {
                        Ok(paths) => app.notify(
                            format!(
                                "Reporte escrito: {}",
                                paths
                                    .iter()
                                    .map(|path| path.display().to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            true,
                        ),
                        Err(err) => app.notify(format!("No se pudo escribir: {}", err), false),
                    }
                }
            }

            ui.add_space(6.0);
            TableBuilder::new(ui)
                .striped(true)
                .column(Column::initial(220.0).at_least(160.0))
                .column(Column::initial(180.0).at_least(120.0))
                .column(Column::exact(70.0))
                .column(Column::exact(70.0))
                .column(Column::exact(70.0))
                .column(Column::exact(90.0))
                .column(Column::exact(80.0))
                .column(Column::exact(90.0))
                .header(22.0, |mut header| {
                    for title in [
                        "Cuenta", "Carpeta", "Servidor", "Nuevos", "Omitidos", "Subidos",
                        "Verificado", "Errores",
                    ] {
                        header.col(|ui| {
                            ui.label(RichText::new(title).strong().small());
                        });
                    }
                })
                .body(|body| {
                    let rows: Vec<(String, crate::report::FolderReport)> = report
                        .accounts
                        .iter()
                        .flat_map(|account| {
                            account
                                .folders
                                .iter()
                                .map(|folder| (account.account.clone(), folder.clone()))
                                .collect::<Vec<_>>()
                        })
                        .collect();
                    body.rows(18.0, rows.len(), |mut row| {
                        let (account, folder) = &rows[row.index()];
                        row.col(|ui| {
                            ui.label(RichText::new(account).small());
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(&folder.folder).small().monospace());
                        });
                        row.col(|ui| {
                            ui.label(
                                RichText::new(
                                    folder
                                        .server_count
                                        .map(|value| value.to_string())
                                        .unwrap_or_else(|| "-".to_string()),
                                )
                                .small(),
                            );
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(folder.new.to_string()).small());
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(folder.skipped.to_string()).small());
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(folder.uploaded.to_string()).small());
                        });
                        row.col(|ui| {
                            let (text, color) = if folder.verified {
                                ("sí", Color32::from_rgb(140, 230, 150))
                            } else {
                                ("no", Color32::from_rgb(255, 215, 0))
                            };
                            ui.label(RichText::new(text).small().color(color));
                        });
                        row.col(|ui| {
                            let color = if folder.errors == 0 {
                                Color32::GRAY
                            } else {
                                Color32::from_rgb(255, 110, 100)
                            };
                            ui.label(RichText::new(folder.errors.to_string()).small().color(color));
                        });
                    });
                });

            let errors: Vec<String> = report
                .accounts
                .iter()
                .flat_map(|account| {
                    account
                        .error_list
                        .iter()
                        .chain(
                            account
                                .folders
                                .iter()
                                .flat_map(|folder| folder.errors_detail.iter()),
                        ).cloned()
                        .collect::<Vec<_>>()
                })
                .collect();
            if !errors.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new("Errores registrados").strong());
                egui::ScrollArea::vertical()
                    .max_height(140.0)
                    .show(ui, |ui| {
                        for error in &errors {
                            ui.label(
                                RichText::new(error)
                                    .small()
                                    .monospace()
                                    .color(Color32::from_rgb(255, 150, 140)),
                            );
                        }
                    });
            }
        });

    app.show_report = open;
}

// ---------------------------------------------------------------------------
// Análisis del origen en segundo plano
// ---------------------------------------------------------------------------

pub fn poll_scan(state: &mut RestoreTabState, logger: &Logger) {
    if let Some(events) = &state.scan_events {
        while let Ok(event) = events.try_recv() {
            match event {
                TaskEvent::ScanProgress {
                    messages,
                    folders,
                    bytes,
                    ..
                } => state.scan_counters = Some((messages, folders, bytes)),
                TaskEvent::Started { detail, .. } => state.scan_status = detail,
                TaskEvent::Finished { summary } => {
                    state.scan_status = summary.summary_line();
                }
                TaskEvent::Account { .. } => {}
            }
        }
    }

    let received = state
        .scan_result
        .as_ref()
        .and_then(|receiver| match receiver.try_recv() {
            Ok(result) => Some(result),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err("El análisis se interrumpió.".to_string())),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
        });

    if let Some(result) = received {
        state.scan_result = None;
        state.scan_events = None;
        state.scanning = false;
        match result {
            Ok(index) => {
                logger.info("scan", format!("Origen analizado: {}", index.summary()));
                state.scan_status = index.summary();
                state.index = Some(index);
            }
            Err(message) => {
                logger.error("scan", format!("No se pudo analizar el origen: {}", message));
                state.scan_status = message;
                state.index = None;
            }
        }
    }
}

/// Resumen del avance de una tarea (cuentas terminadas sobre el total).
pub fn running_summary(task: &RunningTask) -> (usize, usize) {
    let finished = task
        .accounts
        .values()
        .filter(|state| matches!(state, AccountRunState::Finished(_)))
        .count();
    (finished, task.accounts.len())
}
