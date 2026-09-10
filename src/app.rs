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

use crate::archiver::Archiver;
use crate::config::{AppConfig, ZipMode};
use crate::imap_client::ImapDownloader;
use crate::imap_uploader::ImapUploader;
use crate::state::{
    AccountEditModal, AccountProgressState, BackupEvent, LogEntry, LogLevel, OverallSummary,
    RestoreConfigState, RestoreEvent,
};
use crate::storage::StorageManager;
use eframe::egui::{self, Color32, ProgressBar, RichText, ScrollArea, Vec2};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Instant;
use tokio::sync::Semaphore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppTab {
    Backup,
    Restore,
}

pub struct ImapBackupApp {
    active_tab: AppTab,

    // Configuración actual de exportación
    config: AppConfig,
    config_file_path: Option<PathBuf>,

    // Estado del proceso de respaldo (Exportación)
    is_running: bool,
    event_receiver: Option<Receiver<BackupEvent>>,
    event_sender: Sender<BackupEvent>,
    logs: Vec<LogEntry>,
    auto_scroll_logs: bool,
    account_states: HashMap<String, AccountProgressState>,
    overall_summary: Option<OverallSummary>,
    account_modal: AccountEditModal,

    // Estado de Importación / Restauración Masiva
    restore_state: RestoreConfigState,
    restore_event_receiver: Option<Receiver<RestoreEvent>>,
    restore_event_sender: Sender<RestoreEvent>,
    restore_logs: Vec<LogEntry>,
    restore_auto_scroll: bool,

    // Feedback temporal en UI
    status_notification: Option<(String, Instant, Color32)>,
}

impl ImapBackupApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = channel();
        let (r_tx, r_rx) = channel();

        // Cargar configuración por defecto si existe config.toml o config.json
        let (initial_config, initial_path) = if let Ok(cfg) = AppConfig::load_from_file("config.toml") {
            (cfg, Some(PathBuf::from("config.toml")))
        } else if let Ok(cfg) = AppConfig::load_from_file("config.json") {
            (cfg, Some(PathBuf::from("config.json")))
        } else {
            (
                AppConfig {
                    output_dir: PathBuf::from("backups"),
                    concurrency_limit: 3,
                    zip_mode: ZipMode::PerAccount,
                    cleanup_raw_after_zip: false,
                    retry_attempts: 3,
                    retry_delay_secs: 5,
                    timeout_secs: 45,
                    skip_existing: true,
                    accounts: Vec::new(),
                },
                None,
            )
        };

        Self {
            active_tab: AppTab::Backup,
            config: initial_config,
            config_file_path: initial_path,
            is_running: false,
            event_receiver: Some(rx),
            event_sender: tx,
            logs: vec![LogEntry::info("Aplicación iniciada. Lista para exportar o importar buzones.")],
            auto_scroll_logs: true,
            account_states: HashMap::new(),
            overall_summary: None,
            account_modal: AccountEditModal::default(),

            restore_state: RestoreConfigState::default(),
            restore_event_receiver: Some(r_rx),
            restore_event_sender: r_tx,
            restore_logs: vec![LogEntry::info(
                "Módulo de Restauración listo. Selecciona un archivo .zip o carpeta de correos para comenzar.",
            )],
            restore_auto_scroll: true,

            status_notification: None,
        }
    }

    fn set_notification(&mut self, text: impl Into<String>, color: Color32) {
        self.status_notification = Some((text.into(), Instant::now(), color));
    }

    /// Inicia el proceso de backup en un hilo Tokio separado sin bloquear la UI
    fn start_backup_process(&mut self) {
        if self.config.accounts.is_empty() {
            self.set_notification("Debe configurar al menos una cuenta de correo.", Color32::RED);
            return;
        }

        self.is_running = true;
        self.overall_summary = None;
        self.account_states.clear();

        for acc in &self.config.accounts {
            self.account_states.insert(
                acc.email.clone(),
                AccountProgressState {
                    status: "En cola...".to_string(),
                    ..Default::default()
                },
            );
        }

        let config_clone = self.config.clone();
        let (tx, rx) = channel();
        self.event_receiver = Some(rx);
        self.event_sender = tx.clone();

        self.logs.push(LogEntry::info("=== INICIANDO PROCESO DE RESPALDO IMAP ==="));

        thread::spawn(move || {
            let start_time = Instant::now();
            let rt = match tokio::runtime::Runtime::new() {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(BackupEvent::Log(LogEntry::error(format!(
                        "Error creando runtime de Tokio: {}",
                        e
                    ))));
                    return;
                }
            };

            rt.block_on(async {
                let storage_mgr = Arc::new(StorageManager::new(config_clone.output_dir.clone()));
                let semaphore = Arc::new(Semaphore::new(config_clone.concurrency_limit));
                let mut tasks = Vec::new();

                for account in config_clone.accounts.clone() {
                    let sem = Arc::clone(&semaphore);
                    let storage = Arc::clone(&storage_mgr);
                    let tx_worker = tx.clone();
                    let retry_attempts = config_clone.retry_attempts;
                    let retry_delay_secs = config_clone.retry_delay_secs;
                    let timeout_secs = config_clone.timeout_secs;
                    let skip_existing = config_clone.skip_existing;
                    let zip_mode = config_clone.zip_mode;
                    let cleanup_raw = config_clone.cleanup_raw_after_zip;
                    let base_dir = config_clone.output_dir.clone();

                    let task = tokio::spawn(async move {
                        let _permit = sem.acquire().await.expect("Error adquiriendo semáforo");
                        let acc_email = account.email.clone();

                        let account_clone = account.clone();
                        let storage_clone = storage.clone();
                        let tx_for_downloader = tx_worker.clone();

                        let stats = tokio::task::spawn_blocking(move || {
                            let downloader = ImapDownloader::new(
                                &account_clone,
                                &storage_clone,
                                Some(tx_for_downloader),
                                retry_attempts,
                                retry_delay_secs,
                                timeout_secs,
                                skip_existing,
                            );
                            downloader.process_account()
                        })
                        .await
                        .unwrap_or_else(|e| crate::imap_client::AccountStats {
                            email: acc_email.clone(),
                            domain: account.get_domain_or_label(),
                            total_folders: 0,
                            total_messages: 0,
                            downloaded_messages: 0,
                            skipped_messages: 0,
                            total_bytes: 0,
                            errors: vec![format!("Fallo en tarea: {}", e)],
                        });

                        let success = stats.errors.is_empty();

                        if zip_mode == ZipMode::PerAccount && success {
                            let domain = account.get_domain_or_label();
                            let account_dir = storage.account_dir(&domain, &account.email);
                            let zip_path =
                                Archiver::get_account_zip_path(&base_dir, &domain, &account.email);

                            let _ = tx_worker.send(BackupEvent::Log(LogEntry::info(format!(
                                "[{}] Comprimiendo a archivo ZIP...",
                                account.email
                            ))));

                            if let Err(e) = Archiver::zip_directory(&account_dir, &zip_path) {
                                let _ = tx_worker.send(BackupEvent::Log(LogEntry::error(format!(
                                    "[{}] Error en compresión ZIP: {}",
                                    account.email, e
                                ))));
                            } else if cleanup_raw {
                                let _ = Archiver::cleanup_directory(&account_dir);
                            }
                        }

                        let _ = tx_worker.send(BackupEvent::AccountFinished {
                            account: account.email.clone(),
                            success,
                            stats: stats.clone(),
                        });

                        stats
                    });

                    tasks.push(task);
                }

                let mut all_stats = Vec::new();
                for t in tasks {
                    if let Ok(st) = t.await {
                        all_stats.push(st);
                    }
                }

                if config_clone.zip_mode == ZipMode::Consolidated {
                    let zip_path = Archiver::get_consolidated_zip_path(&config_clone.output_dir);
                    let _ = tx.send(BackupEvent::Log(LogEntry::info(format!(
                        "Generando archivo ZIP consolidado maestro en: {}",
                        zip_path.display()
                    ))));

                    if let Err(e) = Archiver::zip_directory(&config_clone.output_dir, &zip_path) {
                        let _ = tx.send(BackupEvent::Log(LogEntry::error(format!(
                            "Error en ZIP consolidado: {}",
                            e
                        ))));
                    } else if config_clone.cleanup_raw_after_zip {
                        for acc in &config_clone.accounts {
                            let domain_dir = config_clone
                                .output_dir
                                .join(sanitize_filename::sanitize(acc.get_domain_or_label()));
                            let _ = Archiver::cleanup_directory(&domain_dir);
                        }
                    }
                }

                let elapsed = start_time.elapsed().as_secs_f64();
                let mut total_downloaded = 0;
                let mut total_skipped = 0;
                let mut total_bytes = 0;
                let mut total_errors = 0;

                for s in &all_stats {
                    total_downloaded += s.downloaded_messages;
                    total_skipped += s.skipped_messages;
                    total_bytes += s.total_bytes;
                    total_errors += s.errors.len();
                }

                let _ = tx.send(BackupEvent::OverallFinished(OverallSummary {
                    total_accounts: all_stats.len(),
                    total_downloaded,
                    total_skipped,
                    total_bytes,
                    total_errors,
                    elapsed_seconds: elapsed,
                }));
            });
        });
    }

    /// Inicia el proceso de restauración masiva IMAP en segundo plano
    fn start_restore_process(&mut self) {
        let source_path = match self.restore_state.source_path.clone() {
            Some(p) => p,
            None => {
                self.set_notification("Selecciona un archivo .zip o carpeta de correos primero.", Color32::RED);
                return;
            }
        };

        let host = self.restore_state.host.trim().to_string();
        let port = match self.restore_state.port.trim().parse::<u16>() {
            Ok(p) => p,
            Err(_) => {
                self.set_notification("Puerto inválido (ej. 993).", Color32::RED);
                return;
            }
        };
        let email = self.restore_state.email.trim().to_string();
        let password = self.restore_state.password.clone();

        if host.is_empty() || email.is_empty() || password.is_empty() {
            self.set_notification("Completa el host, correo y contraseña de destino.", Color32::RED);
            return;
        }

        self.restore_state.is_running = true;
        self.restore_state.status = "Iniciando restauración masiva...".to_string();
        self.restore_state.progress_current = 0;
        self.restore_state.progress_total = 0;
        self.restore_state.last_summary = None;

        let (tx, rx) = channel();
        self.restore_event_receiver = Some(rx);
        self.restore_event_sender = tx.clone();

        self.restore_logs.push(LogEntry::info(format!(
            "=== INICIANDO SUBIDA MASIVA A '{}' ({}:{}) ===",
            email, host, port
        )));

        thread::spawn(move || {
            let uploader = ImapUploader::new(
                host,
                port,
                email,
                password,
                45,
                3,
                3,
                Some(tx),
            );
            uploader.process_restore(&source_path);
        });
    }

    fn process_events(&mut self) {
        let mut events = Vec::new();
        if let Some(ref rx) = self.event_receiver {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }

        for event in events {
            match event {
                BackupEvent::Log(entry) => {
                    self.logs.push(entry);
                }
                BackupEvent::AccountStarted { account } => {
                    if let Some(state) = self.account_states.get_mut(&account) {
                        state.status = "Conectando y descargando...".to_string();
                    }
                }
                BackupEvent::FolderProgress {
                    account,
                    folder,
                    current,
                    total,
                } => {
                    if let Some(state) = self.account_states.get_mut(&account) {
                        state.current_folder = folder;
                        state.current_msgs = current;
                        state.total_msgs = total;
                        state.status = format!("Descargando {}", state.current_folder);
                    }
                }
                BackupEvent::AccountFinished {
                    account,
                    success,
                    stats,
                } => {
                    if let Some(state) = self.account_states.get_mut(&account) {
                        state.is_finished = true;
                        state.has_error = !success;
                        state.status = if success {
                            format!(
                                "Completado ({} msgs, {:.2} MB)",
                                stats.downloaded_messages + stats.skipped_messages,
                                stats.total_bytes as f64 / (1024.0 * 1024.0)
                            )
                        } else {
                            format!("Fallido con {} errores", stats.errors.len())
                        };
                    }
                }
                BackupEvent::OverallFinished(summary) => {
                    self.is_running = false;
                    self.overall_summary = Some(summary.clone());
                    self.logs.push(LogEntry::success(format!(
                        "=== RESPALDO COMPLETADO EN {:.2}s ({} cuentas, {} msgs, {:.2} MB, {} errores) ===",
                        summary.elapsed_seconds,
                        summary.total_accounts,
                        summary.total_downloaded,
                        summary.total_bytes as f64 / (1024.0 * 1024.0),
                        summary.total_errors
                    )));
                    self.set_notification("¡Proceso de copia de seguridad completado!", Color32::GREEN);
                }
            }
        }
    }

    fn process_restore_events(&mut self) {
        let mut events = Vec::new();
        if let Some(ref rx) = self.restore_event_receiver {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }

        for event in events {
            match event {
                RestoreEvent::Log(entry) => {
                    self.restore_logs.push(entry);
                }
                RestoreEvent::Progress {
                    folder,
                    current,
                    total,
                    ..
                } => {
                    self.restore_state.current_folder = folder.clone();
                    self.restore_state.progress_current = current;
                    self.restore_state.progress_total = total;
                    self.restore_state.status = format!("Subiendo a '{}' ({}/{})", folder, current, total);
                }
                RestoreEvent::Finished(summary) => {
                    self.restore_state.is_running = false;
                    self.restore_state.status = format!(
                        "Restauración finalizada: {} correos subidos ({:.2} MB)",
                        summary.total_uploaded,
                        summary.total_bytes as f64 / (1024.0 * 1024.0)
                    );
                    self.restore_state.last_summary = Some(summary);
                    self.set_notification("¡Restauración masiva completada con éxito!", Color32::GREEN);
                }
            }
        }
    }
}

impl eframe::App for ImapBackupApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_events();
        self.process_restore_events();

        if self.is_running || self.restore_state.is_running {
            ctx.request_repaint();
        }

        // Panel Superior: Barra de Título, Notificaciones y Pestañas de Navegación
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(RichText::new("📦 IMAP Backup & Migration Suite").strong().color(Color32::from_rgb(100, 200, 255)));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some((ref msg, instant, color)) = self.status_notification {
                        if instant.elapsed().as_secs() < 6 {
                            ui.label(RichText::new(msg).color(color).strong());
                        }
                    }
                });
            });

            ui.add_space(6.0);

            // Barra de Pestañas Principales
            ui.horizontal(|ui| {
                let backup_btn = ui.selectable_label(self.active_tab == AppTab::Backup, RichText::new("📥 Respaldar / Exportar Cuentas").size(15.0).strong());
                if backup_btn.clicked() {
                    self.active_tab = AppTab::Backup;
                }

                let restore_btn = ui.selectable_label(self.active_tab == AppTab::Restore, RichText::new("📤 Restaurar / Importar Masivo").size(15.0).strong());
                if restore_btn.clicked() {
                    self.active_tab = AppTab::Restore;
                }
            });

            ui.separator();

            // Opciones contextuales de la barra superior según pestaña activa
            match self.active_tab {
                AppTab::Backup => {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("📂 Cargar Config").clicked() {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("Configuraciones", &["toml", "json"])
                                .pick_file()
                            {
                                match AppConfig::load_from_file(&path) {
                                    Ok(loaded) => {
                                        self.config = loaded;
                                        self.config_file_path = Some(path.clone());
                                        self.set_notification(format!("Configuración cargada desde {}", path.display()), Color32::GREEN);
                                    }
                                    Err(e) => {
                                        self.set_notification(format!("Error cargando archivo: {}", e), Color32::RED);
                                    }
                                }
                            }
                        }

                        if ui.button("💾 Guardar Config").clicked() {
                            let target = self.config_file_path.clone().unwrap_or_else(|| PathBuf::from("config.toml"));
                            let ext = target.extension().and_then(|s| s.to_str()).unwrap_or("toml");
                            let res = if ext == "json" {
                                serde_json::to_string_pretty(&self.config).map_err(|e| anyhow::anyhow!(e))
                            } else {
                                toml::to_string_pretty(&self.config).map_err(|e| anyhow::anyhow!(e))
                            };

                            match res {
                                Ok(content) => {
                                    if std::fs::write(&target, content).is_ok() {
                                        self.config_file_path = Some(target.clone());
                                        self.set_notification(format!("Guardado en {}", target.display()), Color32::GREEN);
                                    }
                                }
                                Err(e) => {
                                    self.set_notification(format!("Error de serialización: {}", e), Color32::RED);
                                }
                            }
                        }

                        ui.separator();
                        ui.label(RichText::new("Destino Backup:").strong());
                        ui.label(RichText::new(self.config.output_dir.display().to_string()).italics().color(Color32::LIGHT_GRAY));
                        if ui.button("📁 Cambiar Carpeta").clicked() {
                            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                                self.config.output_dir = folder;
                            }
                        }
                    });

                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label("Concurrencia:");
                        ui.add(egui::Slider::new(&mut self.config.concurrency_limit, 1..=10).text("cuentas"));
                        ui.separator();
                        ui.label("Modo ZIP:");
                        egui::ComboBox::from_id_source("zip_mode_combo")
                            .selected_text(match self.config.zip_mode {
                                ZipMode::PerAccount => "Un ZIP por cuenta",
                                ZipMode::Consolidated => "ZIP Maestro Consolidado",
                                ZipMode::None => "Sin ZIP (carpetas sueltas)",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.config.zip_mode, ZipMode::PerAccount, "Un ZIP por cuenta");
                                ui.selectable_value(&mut self.config.zip_mode, ZipMode::Consolidated, "ZIP Maestro Consolidado");
                                ui.selectable_value(&mut self.config.zip_mode, ZipMode::None, "Sin ZIP (carpetas sueltas)");
                            });
                        ui.separator();
                        ui.checkbox(&mut self.config.cleanup_raw_after_zip, "Limpiar .eml tras ZIP");
                        ui.checkbox(&mut self.config.skip_existing, "Sincronización incremental");
                    });
                }
                AppTab::Restore => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("ℹ Modo de Restauración:").strong().color(Color32::LIGHT_BLUE));
                        ui.label("Sube miles de correos y recrea subcarpetas directamente al buzón IMAP sin los límites de Roundcube.");
                    });
                }
            }

            ui.add_space(6.0);
        });

        // Panel Inferior: Botón de Acción, Progreso y Consola de Logs
        egui::TopBottomPanel::bottom("bottom_panel").min_height(230.0).show(ctx, |ui| {
            ui.add_space(4.0);

            match self.active_tab {
                AppTab::Backup => {
                    // Controles inferiores de Exportación
                    ui.horizontal(|ui| {
                        let btn_text = if self.is_running {
                            "⏳ Procesando Copias de Seguridad..."
                        } else {
                            "🚀 Iniciar Backup de Todas las Cuentas"
                        };

                        let btn = egui::Button::new(RichText::new(btn_text).size(16.0).strong())
                            .fill(if self.is_running { Color32::from_rgb(70, 70, 70) } else { Color32::from_rgb(34, 139, 34) });

                        if ui.add_enabled(!self.is_running, btn).clicked() {
                            self.start_backup_process();
                        }

                        if let Some(ref sum) = self.overall_summary {
                            ui.label(RichText::new(format!(
                                "✔ Último backup: {} correos ({:.2} MB) en {:.1}s",
                                sum.total_downloaded,
                                sum.total_bytes as f64 / (1024.0 * 1024.0),
                                sum.elapsed_seconds
                            )).color(Color32::GREEN).strong());
                        }
                    });

                    ui.add_space(4.0);
                    ui.separator();

                    ui.horizontal(|ui| {
                        ui.label(RichText::new("📟 Consola de Actividad de Exportación").strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("🗑 Limpiar").clicked() {
                                self.logs.clear();
                            }
                            ui.checkbox(&mut self.auto_scroll_logs, "Auto-scroll");
                        });
                    });

                    ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .stick_to_bottom(self.auto_scroll_logs)
                        .show(ui, |ui| {
                            for log in &self.logs {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&log.timestamp).color(Color32::DARK_GRAY).monospace());
                                    let (badge, color) = match log.level {
                                        LogLevel::Info => ("[INFO]", Color32::from_rgb(180, 220, 255)),
                                        LogLevel::Warning => ("[WARN]", Color32::from_rgb(255, 215, 0)),
                                        LogLevel::Error => ("[ERROR]", Color32::from_rgb(255, 99, 71)),
                                        LogLevel::Success => ("[OK]", Color32::from_rgb(144, 238, 144)),
                                    };
                                    ui.label(RichText::new(badge).color(color).strong().monospace());
                                    ui.label(RichText::new(&log.message).color(color).monospace());
                                });
                            }
                        });
                }
                AppTab::Restore => {
                    // Controles inferiores de Importación Masiva
                    ui.horizontal(|ui| {
                        let btn_text = if self.restore_state.is_running {
                            "⏳ Subiendo Correos al Servidor..."
                        } else {
                            "🚀 Iniciar Restauración Masiva al Buzón"
                        };

                        let can_start = !self.restore_state.is_running && self.restore_state.source_path.is_some();
                        let btn = egui::Button::new(RichText::new(btn_text).size(16.0).strong())
                            .fill(if self.restore_state.is_running {
                                Color32::from_rgb(70, 70, 70)
                            } else if can_start {
                                Color32::from_rgb(34, 139, 34)
                            } else {
                                Color32::from_rgb(60, 60, 60)
                            });

                        if ui.add_enabled(can_start, btn).clicked() {
                            self.start_restore_process();
                        }

                        if self.restore_state.is_running && self.restore_state.progress_total > 0 {
                            let frac = (self.restore_state.progress_current as f32 / self.restore_state.progress_total as f32).clamp(0.0, 1.0);
                            ui.add(ProgressBar::new(frac).show_percentage().text(format!("{}/{} correos ({})", self.restore_state.progress_current, self.restore_state.progress_total, self.restore_state.current_folder)));
                        } else if let Some(ref sum) = self.restore_state.last_summary {
                            ui.label(RichText::new(format!(
                                "✔ Subidos {} correos en {:.1}s ({} errores)",
                                sum.total_uploaded, sum.elapsed_seconds, sum.total_errors
                            )).color(if sum.total_errors == 0 { Color32::GREEN } else { Color32::YELLOW }).strong());
                        }
                    });

                    ui.add_space(4.0);
                    ui.separator();

                    ui.horizontal(|ui| {
                        ui.label(RichText::new("📟 Consola de Restauración e Inyección IMAP").strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("🗑 Limpiar").clicked() {
                                self.restore_logs.clear();
                            }
                            ui.checkbox(&mut self.restore_auto_scroll, "Auto-scroll");
                        });
                    });

                    ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .stick_to_bottom(self.restore_auto_scroll)
                        .show(ui, |ui| {
                            for log in &self.restore_logs {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&log.timestamp).color(Color32::DARK_GRAY).monospace());
                                    let (badge, color) = match log.level {
                                        LogLevel::Info => ("[INFO]", Color32::from_rgb(180, 220, 255)),
                                        LogLevel::Warning => ("[WARN]", Color32::from_rgb(255, 215, 0)),
                                        LogLevel::Error => ("[ERROR]", Color32::from_rgb(255, 99, 71)),
                                        LogLevel::Success => ("[OK]", Color32::from_rgb(144, 238, 144)),
                                    };
                                    ui.label(RichText::new(badge).color(color).strong().monospace());
                                    ui.label(RichText::new(&log.message).color(color).monospace());
                                });
                            }
                        });
                }
            }
        });

        // Panel Central: Vistas Principales según la Pestaña
        egui::CentralPanel::default().show(ctx, |ui| {
            match self.active_tab {
                AppTab::Backup => {
                    // Vista de Exportación / Cuentas
                    ui.horizontal(|ui| {
                        ui.heading(format!("Buzones a Respaldar ({})", self.config.accounts.len()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button(RichText::new("➕ Añadir Cuenta").strong().color(Color32::GREEN)).clicked() {
                                self.account_modal = AccountEditModal::open_for_new();
                            }
                        });
                    });

                    ui.add_space(6.0);

                    if self.config.accounts.is_empty() {
                        ui.centered_and_justified(|ui| {
                            ui.label(RichText::new("No hay cuentas configuradas.\nHaz clic en '➕ Añadir Cuenta' o carga un archivo JSON/TOML.")
                                .italics().size(15.0).color(Color32::GRAY));
                        });
                        return;
                    }

                    ScrollArea::vertical().show(ui, |ui| {
                        let mut account_to_delete = None;
                        let mut account_to_edit = None;

                        for (idx, acc) in self.config.accounts.iter().enumerate() {
                            ui.group(|ui| {
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(RichText::new(&acc.email).strong().size(14.0));
                                            if let Some(ref lbl) = acc.label {
                                                ui.label(RichText::new(format!("({})", lbl)).color(Color32::LIGHT_BLUE));
                                            }
                                        });
                                        ui.label(RichText::new(format!("Host: {}:{} | TLS: {}", acc.host, acc.port, acc.tls)).color(Color32::GRAY));
                                    });

                                    if let Some(state) = self.account_states.get(&acc.email) {
                                        ui.separator();
                                        ui.vertical(|ui| {
                                            let status_color = if state.has_error {
                                                Color32::RED
                                            } else if state.is_finished {
                                                Color32::GREEN
                                            } else {
                                                Color32::YELLOW
                                            };
                                            ui.label(RichText::new(&state.status).color(status_color).strong());

                                            if state.total_msgs > 0 && !state.is_finished {
                                                let frac = (state.current_msgs as f32 / state.total_msgs as f32).clamp(0.0, 1.0);
                                                ui.add(ProgressBar::new(frac).show_percentage().text(format!("{}/{} msgs", state.current_msgs, state.total_msgs)));
                                            }
                                        });
                                    }

                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.button("🗑 Eliminar").clicked() {
                                            account_to_delete = Some(idx);
                                        }
                                        if ui.button("✏ Editar").clicked() {
                                            account_to_edit = Some((idx, acc.clone()));
                                        }
                                    });
                                });
                            });
                            ui.add_space(2.0);
                        }

                        if let Some(idx) = account_to_delete {
                            self.config.accounts.remove(idx);
                            self.set_notification("Cuenta eliminada.", Color32::YELLOW);
                        }

                        if let Some((idx, acc)) = account_to_edit {
                            self.account_modal = AccountEditModal::open_for_edit(idx, &acc);
                        }
                    });
                }

                AppTab::Restore => {
                    // Vista de Importación / Restauración Masiva
                    ui.heading("Restaurar Backup a Buzón de Destino");
                    ui.add_space(4.0);
                    ui.label("Configura los datos del buzón de destino y selecciona el archivo .ZIP o la carpeta con tus correos respaldados.");

                    ui.add_space(10.0);

                    // Formulario de Credenciales de Destino
                    ui.group(|ui| {
                        ui.label(RichText::new("1. Datos del Servidor de Destino (Hostinger / cPanel / IMAP)").strong());
                        ui.add_space(4.0);

                        egui::Grid::new("restore_dest_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                            ui.label("Servidor IMAP:");
                            if ui.text_edit_singleline(&mut self.restore_state.host).changed() {
                                self.restore_state.host.retain(|c| c != '\r' && c != '\n');
                            }
                            ui.end_row();

                            ui.label("Puerto:");
                            ui.horizontal(|ui| {
                                ui.text_edit_singleline(&mut self.restore_state.port);
                                ui.checkbox(&mut self.restore_state.tls, "Usar SSL/TLS");
                            });
                            ui.end_row();

                            ui.label("Correo / Usuario Destino:");
                            if ui.text_edit_singleline(&mut self.restore_state.email).changed() {
                                self.restore_state.email.retain(|c| c != '\r' && c != '\n');
                            }
                            ui.end_row();

                            ui.label("Contraseña Destino:");
                            if ui.add(egui::TextEdit::singleline(&mut self.restore_state.password).password(true)).changed() {
                                self.restore_state.password.retain(|c| c != '\r' && c != '\n');
                            }
                            ui.end_row();
                        });
                    });

                    ui.add_space(10.0);

                    // Selector de Origen de Backup
                    ui.group(|ui| {
                        ui.label(RichText::new("2. Origen del Respaldo (Archivo .ZIP o Carpeta .EML)").strong());
                        ui.add_space(4.0);

                        ui.horizontal(|ui| {
                            if ui.button(RichText::new("📦 Seleccionar Archivo .ZIP").strong()).clicked() {
                                if let Some(zip_file) = rfd::FileDialog::new()
                                    .add_filter("Archivos ZIP", &["zip"])
                                    .pick_file()
                                {
                                    self.restore_state.source_path = Some(zip_file.clone());
                                    // Escanear correos en el archivo ZIP
                                    if let Ok(map) = ImapUploader::scan_backup_source(&zip_file) {
                                        self.restore_state.detected_folders = map.len();
                                        self.restore_state.detected_emails = map.values().map(|v| v.len()).sum();
                                        self.set_notification(
                                            format!("Detectados {} correos en {} carpetas", self.restore_state.detected_emails, self.restore_state.detected_folders),
                                            Color32::GREEN,
                                        );
                                    }
                                }
                            }

                            if ui.button(RichText::new("📁 Seleccionar Carpeta de Correos").strong()).clicked() {
                                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                                    self.restore_state.source_path = Some(folder.clone());
                                    // Escanear correos en la carpeta
                                    if let Ok(map) = ImapUploader::scan_backup_source(&folder) {
                                        self.restore_state.detected_folders = map.len();
                                        self.restore_state.detected_emails = map.values().map(|v| v.len()).sum();
                                        self.set_notification(
                                            format!("Detectados {} correos en {} carpetas", self.restore_state.detected_emails, self.restore_state.detected_folders),
                                            Color32::GREEN,
                                        );
                                    }
                                }
                            }
                        });

                        ui.add_space(6.0);

                        if let Some(ref path) = self.restore_state.source_path {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Ruta seleccionada:").strong());
                                ui.label(RichText::new(path.display().to_string()).color(Color32::LIGHT_GREEN));
                            });

                            if self.restore_state.detected_emails > 0 {
                                ui.label(RichText::new(format!(
                                    "✔ Listo para transferir: {} correos distribuidos en {} carpetas (se recrearán automáticamente)",
                                    self.restore_state.detected_emails, self.restore_state.detected_folders
                                )).color(Color32::from_rgb(100, 220, 255)).strong());
                            }
                        } else {
                            ui.label(RichText::new("Ningún archivo o carpeta seleccionado aún.")
                                .italics().color(Color32::GRAY));
                        }
                    });
                }
            }
        });

        // Diálogo Modal Flotante: Añadir / Editar Cuenta de Exportación
        if self.account_modal.is_open {
            egui::Window::new(if self.account_modal.edit_index.is_some() { "Editar Cuenta IMAP" } else { "Añadir Cuenta IMAP" })
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.set_min_width(380.0);

                    if let Some(ref err) = self.account_modal.error_msg {
                        ui.label(RichText::new(err).color(Color32::RED).strong());
                        ui.add_space(4.0);
                    }

                    egui::Grid::new("account_edit_grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                        ui.label("Correo / Usuario:");
                        if ui.text_edit_singleline(&mut self.account_modal.email).changed() {
                            self.account_modal.email.retain(|c| c != '\r' && c != '\n');
                        }
                        ui.end_row();

                        ui.label("Contraseña:");
                        if ui.add(egui::TextEdit::singleline(&mut self.account_modal.password).password(true)).changed() {
                            self.account_modal.password.retain(|c| c != '\r' && c != '\n');
                        }
                        ui.end_row();

                        ui.label("Servidor IMAP:");
                        if ui.text_edit_singleline(&mut self.account_modal.host).changed() {
                            self.account_modal.host.retain(|c| c != '\r' && c != '\n');
                        }
                        ui.end_row();

                        ui.label("Puerto:");
                        ui.text_edit_singleline(&mut self.account_modal.port);
                        ui.end_row();

                        ui.label("Usar TLS (SSL):");
                        ui.checkbox(&mut self.account_modal.tls, "Habilitado (Puerto 993)");
                        ui.end_row();

                        ui.label("Etiqueta / Dominio:");
                        ui.text_edit_singleline(&mut self.account_modal.label);
                        ui.end_row();

                        ui.label("Excluir Carpetas:");
                        ui.text_edit_singleline(&mut self.account_modal.exclude_folders);
                        ui.end_row();
                    });

                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Guardar").clicked() {
                            match self.account_modal.to_account_config() {
                                Ok(acc) => {
                                    if let Some(idx) = self.account_modal.edit_index {
                                        self.config.accounts[idx] = acc;
                                        self.set_notification("Cuenta actualizada.", Color32::GREEN);
                                    } else {
                                        self.config.accounts.push(acc);
                                        self.set_notification("Nueva cuenta añadida.", Color32::GREEN);
                                    }
                                    self.account_modal.is_open = false;
                                }
                                Err(err) => {
                                    self.account_modal.error_msg = Some(err);
                                }
                            }
                        }

                        if ui.button("Cancelar").clicked() {
                            self.account_modal.is_open = false;
                        }
                    });
                });
        }
    }
}
