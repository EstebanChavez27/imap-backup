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
use crate::state::{
    AccountEditModal, AccountProgressState, BackupEvent, LogEntry, LogLevel, OverallSummary,
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

pub struct ImapBackupApp {
    // Configuración actual
    config: AppConfig,
    config_file_path: Option<PathBuf>,

    // Estado del proceso de respaldo
    is_running: bool,
    event_receiver: Option<Receiver<BackupEvent>>,
    event_sender: Sender<BackupEvent>,

    // Logs y estados visuales
    logs: Vec<LogEntry>,
    auto_scroll_logs: bool,
    account_states: HashMap<String, AccountProgressState>,
    overall_summary: Option<OverallSummary>,

    // Modal de edición/creación de cuenta
    account_modal: AccountEditModal,

    // Feedback temporal en UI
    status_notification: Option<(String, Instant, Color32)>,
}

impl ImapBackupApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = channel();

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
            config: initial_config,
            config_file_path: initial_path,
            is_running: false,
            event_receiver: Some(rx),
            event_sender: tx,
            logs: vec![LogEntry::info("Aplicación iniciada. Lista para realizar copias de seguridad.")],
            auto_scroll_logs: true,
            account_states: HashMap::new(),
            overall_summary: None,
            account_modal: AccountEditModal::default(),
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

        // Lanzar hilo en background para orquestación asíncrona
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

                        // Ejecutar descarga IMAP
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

                        // Compresión ZIP por cuenta
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

                // Esperar a que todas las tareas finalicen
                let mut all_stats = Vec::new();
                for t in tasks {
                    if let Ok(st) = t.await {
                        all_stats.push(st);
                    }
                }

                // Compresión ZIP consolidada si está configurada
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

    /// Procesa los eventos emitidos por el hilo de segundo plano
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
                    self.set_notification(
                        "¡Proceso de copia de seguridad completado!",
                        Color32::GREEN,
                    );
                }
            }
        }
    }
}

impl eframe::App for ImapBackupApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_events();

        if self.is_running {
            ctx.request_repaint();
        }

        // Barra Superior: Gestión de Archivo y Carpeta de Salida
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(RichText::new("📦 IMAP Backup & Migration Tool").strong().color(Color32::from_rgb(100, 200, 255)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some((ref msg, instant, color)) = self.status_notification {
                        if instant.elapsed().as_secs() < 6 {
                            ui.label(RichText::new(msg).color(color).strong());
                        }
                    }
                });
            });

            ui.separator();

            ui.horizontal_wrapped(|ui| {
                // Cargar Configuración
                if ui.button("📂 Cargar Config").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Configuraciones", &["toml", "json"])
                        .pick_file()
                    {
                        match AppConfig::load_from_file(&path) {
                            Ok(loaded) => {
                                self.config = loaded;
                                self.config_file_path = Some(path.clone());
                                self.set_notification(
                                    format!("Configuración cargada desde {}", path.display()),
                                    Color32::GREEN,
                                );
                            }
                            Err(e) => {
                                self.set_notification(format!("Error cargando archivo: {}", e), Color32::RED);
                            }
                        }
                    }
                }

                // Guardar Configuración
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
                                self.set_notification(format!("Configuración guardada en {}", target.display()), Color32::GREEN);
                            } else {
                                self.set_notification("Error al escribir el archivo", Color32::RED);
                            }
                        }
                        Err(e) => {
                            self.set_notification(format!("Error de serialización: {}", e), Color32::RED);
                        }
                    }
                }

                // Guardar Como...
                if ui.button("💾 Guardar Como...").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("TOML", &["toml"])
                        .add_filter("JSON", &["json"])
                        .set_file_name("config.toml")
                        .save_file()
                    {
                        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("toml");
                        let content_res = if ext == "json" {
                            serde_json::to_string_pretty(&self.config).map_err(|e| anyhow::anyhow!(e))
                        } else {
                            toml::to_string_pretty(&self.config).map_err(|e| anyhow::anyhow!(e))
                        };

                        if let Ok(content) = content_res {
                            if std::fs::write(&path, content).is_ok() {
                                self.config_file_path = Some(path.clone());
                                self.set_notification(format!("Guardado en {}", path.display()), Color32::GREEN);
                            }
                        }
                    }
                }

                ui.separator();

                // Selector de Carpeta de Destino
                ui.label(RichText::new("Destino:").strong());
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
                ui.checkbox(&mut self.config.cleanup_raw_after_zip, "Limpiar .eml tras comprimir");
                ui.checkbox(&mut self.config.skip_existing, "Sincronización incremental");
            });

            ui.add_space(6.0);
        });

        // Panel Inferior: Consola de Logs y Barra de Progreso Principal
        egui::TopBottomPanel::bottom("bottom_panel").min_height(220.0).show(ctx, |ui| {
            ui.add_space(4.0);

            // Botón Principal de Ejecución
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

            // Encabezado de Consola
            ui.horizontal(|ui| {
                ui.label(RichText::new("📟 Consola de Actividad en Tiempo Real").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("🗑 Limpiar").clicked() {
                        self.logs.clear();
                    }
                    ui.checkbox(&mut self.auto_scroll_logs, "Auto-scroll");
                });
            });

            // Visor de Logs con Scroll
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
        });

        // Panel Central: Lista y Gestión de Cuentas IMAP
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(format!("Buzones Configurados ({})", self.config.accounts.len()));
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

            // Tabla de Cuentas
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

                            // Estado de Progreso en Tiempo Real
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

                            // Botones de Acción
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
        });

        // Diálogo Modal Flotante: Añadir / Editar Cuenta
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
                        ui.text_edit_singleline(&mut self.account_modal.email);
                        ui.end_row();

                        ui.label("Contraseña:");
                        ui.add(egui::TextEdit::singleline(&mut self.account_modal.password).password(true));
                        ui.end_row();

                        ui.label("Servidor IMAP:");
                        ui.text_edit_singleline(&mut self.account_modal.host);
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
