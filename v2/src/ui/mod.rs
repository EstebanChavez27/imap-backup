// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Interfaz gráfica (egui/eframe).
//!
//! La UI es una capa fina sobre los motores: arranca tareas en un hilo aparte,
//! consume eventos por canal y muestra progreso, logs y reportes. Nunca bloquea el
//! hilo de dibujado, y todas las operaciones largas se pueden cancelar.

mod tabs;

use crate::cancel::CancelToken;
use crate::config::{discover_config_path, secrets_path, AppConfig, Secrets};
use crate::connect::RemoteFolder;
use crate::events::{AccountRunState, Ctx, LogEntry, LogLevel, Logger, TaskEvent, TaskKind};
use crate::report::RunReport;
use eframe::egui::{self, Color32, RichText, Vec2, ViewportBuilder};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Instant;

pub use tabs::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(Default)]
pub(crate) enum Tab {
    #[default]
    Backup,
    Restore,
    Migrate,
}


/// Tarea en ejecución: el canal de eventos, el token de cancelación y el estado
/// acumulado que la UI necesita para pintar progreso.
pub struct RunningTask {
    pub kind: TaskKind,
    pub receiver: Receiver<TaskEvent>,
    pub cancel: CancelToken,
    pub started: Instant,
    pub accounts: BTreeMap<String, AccountRunState>,
    pub summary: Option<RunReport>,
    pub scan: Option<(u64, u64, u64)>,
    pub detail: String,
}

impl RunningTask {
    pub fn is_finished(&self) -> bool {
        self.summary.is_some()
    }
}

/// Estado del sondeo de un servidor ("Probar conexión" / listar carpetas).
#[derive(Default)]
pub enum ProbeState {
    #[default]
    Idle,
    Running { account: String },
    Loaded { account: String, folders: Vec<RemoteFolder> },
    Error { account: String, message: String },
}


/// Resultado que envía el hilo de sondeo.
pub struct ProbeResult {
    pub account: String,
    pub folders: Option<Vec<RemoteFolder>>,
    pub error: Option<String>,
}

pub struct App {
    pub(crate) tab: Tab,
    pub(crate) logger: Arc<Logger>,
    pub(crate) cancel: CancelToken,
    pub(crate) log_cursor: u64,
    pub(crate) log_lines: Vec<LogEntry>,
    pub(crate) log_filter: String,
    pub(crate) log_min_level: LogLevel,
    pub(crate) auto_scroll: bool,
    pub(crate) config: AppConfig,
    pub(crate) secrets: Secrets,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) secrets_path: PathBuf,
    pub(crate) notification: Option<(String, Instant, bool)>,
    pub(crate) running: Option<RunningTask>,
    pub(crate) probe: ProbeState,
    pub(crate) probe_rx: Option<Receiver<ProbeResult>>,
    pub(crate) account_modal: Option<tabs::AccountModal>,
    pub(crate) backup_state: tabs::BackupTabState,
    pub(crate) restore_state: tabs::RestoreTabState,
    pub(crate) migrate_state: tabs::MigrateTabState,
    pub(crate) last_report: Option<RunReport>,
    pub(crate) show_report: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedUi {
    version: u32,
    tab: Tab,
    config_path: Option<String>,
    auto_scroll: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.window_rounding = 8.0.into();
        visuals.menu_rounding = 6.0.into();
        visuals.widgets.noninteractive.rounding = 5.0.into();
        visuals.widgets.inactive.rounding = 5.0.into();
        visuals.widgets.hovered.rounding = 5.0.into();
        visuals.widgets.active.rounding = 5.0.into();
        cc.egui_ctx.set_visuals(visuals);

        let persisted: Option<PersistedUi> = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, eframe::APP_KEY));

        let config_path = persisted
            .as_ref()
            .and_then(|state| state.config_path.as_ref().map(PathBuf::from))
            .filter(|path| path.exists())
            .or_else(|| discover_config_path(None));

        let logger = Arc::new(Logger::with_capacity(6000));
        logger.info(
            "app",
            format!(
                "IMAP Backup {} — interfaz iniciada.",
                crate::VERSION
            ),
        );

        let (config, config_error) = match &config_path {
            Some(path) => match AppConfig::load_from_file(path) {
                Ok(config) => {
                    logger.info("app", format!("Configuración cargada: {}", path.display()));
                    (config, None)
                }
                Err(err) => {
                    logger.error("app", format!("No se pudo cargar {}: {:#}", path.display(), err));
                    (AppConfig::default(), Some(err.to_string()))
                }
            },
            None => {
                logger.warn(
                    "app",
                    "No se encontró configuración. Se puede crear una desde 'Nueva configuración' o cargar un archivo existente.",
                );
                (AppConfig::default(), None)
            }
        };

        let secrets_path = secrets_path(config_path.as_deref());
        let secrets = Secrets::load(&secrets_path);
        if !secrets.passwords.is_empty() {
            logger.info(
                "app",
                format!(
                    "Contraseñas cargadas desde {} ({} cuenta(s)).",
                    secrets_path.display(),
                    secrets.passwords.len()
                ),
            );
        }
        for warning in config.warnings() {
            logger.warn("config", warning);
        }
        let _ = config_error;

        Self {
            tab: persisted.as_ref().map(|state| state.tab).unwrap_or_default(),
            logger,
            cancel: CancelToken::new(),
            log_cursor: 0,
            log_lines: Vec::new(),
            log_filter: String::new(),
            log_min_level: LogLevel::Info,
            auto_scroll: persisted.as_ref().map(|state| state.auto_scroll).unwrap_or(true),
            config,
            secrets,
            config_path,
            secrets_path,
            notification: None,
            running: None,
            probe: ProbeState::Idle,
            probe_rx: None,
            account_modal: None,
            backup_state: tabs::BackupTabState::default(),
            restore_state: tabs::RestoreTabState::default(),
            migrate_state: tabs::MigrateTabState::default(),
            last_report: None,
            show_report: false,
        }
    }

    pub(crate) fn notify(&mut self, message: impl Into<String>, ok: bool) {
        self.notification = Some((message.into(), Instant::now(), ok));
    }

    fn is_busy(&self) -> bool {
        self.running
            .as_ref()
            .map(|task| !task.is_finished())
            .unwrap_or(false)
    }

    /// Copia los logs nuevos del logger al buffer de la UI.
    fn pull_logs(&mut self) {
        let new_entries = self.logger.snapshot_from(&mut self.log_cursor);
        if new_entries.is_empty() {
            return;
        }
        self.log_lines.extend(new_entries);
        // La UI tampoco guarda un historial infinito: el logger ya recorta.
        let max = self.logger.capacity();
        if self.log_lines.len() > max {
            let excess = self.log_lines.len() - max;
            self.log_lines.drain(0..excess);
        }
    }

    fn drain_task_events(&mut self) {
        let mut finished: Option<RunReport> = None;
        if let Some(task) = self.running.as_mut() {
            while let Ok(event) = task.receiver.try_recv() {
                match event {
                    TaskEvent::Started { kind, detail } => {
                        task.kind = kind;
                        task.detail = detail;
                    }
                    TaskEvent::Account { account, state } => {
                        task.accounts.insert(account, state);
                    }
                    TaskEvent::ScanProgress {
                        messages,
                        folders,
                        bytes,
                        ..
                    } => {
                        task.scan = Some((messages, folders, bytes));
                    }
                    TaskEvent::Finished { summary } => {
                        task.summary = Some(*summary);
                    }
                }
            }
            if let Some(summary) = task.summary.clone() {
                finished = Some(summary);
            }
        }

        if let Some(summary) = finished {
            self.last_report = Some(summary.clone());
            let ok = !summary.has_errors() && !summary.cancelled;
            self.notify(summary.summary_line(), ok);
            if let Some(task) = self.running.take() {
                self.logger.info(
                    "app",
                    format!("{} finalizado en {:.1}s.", task.kind.label(), task.started.elapsed().as_secs_f64()),
                );
            }
        }
    }

    fn poll_probe(&mut self) {
        let Some(receiver) = self.probe_rx.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok(result) => {
                self.probe = match (result.folders, result.error) {
                    (Some(folders), _) => {
                        self.notify(
                            format!("{} carpeta(s) en {}", folders.len(), result.account),
                            true,
                        );
                        ProbeState::Loaded {
                            account: result.account,
                            folders,
                        }
                    }
                    (None, Some(message)) => {
                        self.notify(format!("Fallo la conexión: {}", message), false);
                        ProbeState::Error {
                            account: result.account,
                            message,
                        }
                    }
                    (None, None) => ProbeState::Idle,
                };
                self.probe_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.probe_rx = None;
                self.probe = ProbeState::Idle;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Lanza el sondeo de conexión de una cuenta en segundo plano.
    pub(crate) fn start_probe(&mut self, email: String) {
        let Some(account) = self.config.account_by_email(&email).cloned() else {
            self.notify("Cuenta no encontrada en la configuración.", false);
            return;
        };
        let timeout = self.config.timeout_secs;
        let password = match account.resolve_password(&self.secrets, false) {
            Ok(password) => password,
            Err(err) => {
                self.notify(format!("Falta la contraseña: {}", err), false);
                return;
            }
        };

        let (tx, rx) = channel();
        self.probe_rx = Some(rx);
        self.probe = ProbeState::Running {
            account: email.clone(),
        };
        self.logger
            .info("conn", format!("Probando conexión con {}...", email));

        std::thread::spawn(move || {
            let mut account = account;
            account.password = Some(password);
            let result = (|| -> anyhow::Result<Vec<RemoteFolder>> {
                let conn = crate::connect::ConnOptions::from_account(&account, &Secrets::default())
                    .map(|conn| conn.with_timeout(timeout))?;
                let mut session = crate::connect::connect_and_login(&conn)?;
                let folders = crate::connect::list_folders(&mut session)?;
                let _ = session.logout();
                Ok(folders)
            })();

            let payload = match result {
                Ok(folders) => ProbeResult {
                    account: email.clone(),
                    folders: Some(folders),
                    error: None,
                },
                Err(err) => ProbeResult {
                    account: email.clone(),
                    folders: None,
                    error: Some(format!("{:#}", err)),
                },
            };
            let _ = tx.send(payload);
        });
    }

    fn start_task<F>(&mut self, kind: TaskKind, detail: String, run: F)
    where
        F: FnOnce(Ctx) -> anyhow::Result<RunReport> + Send + 'static,
    {
        if self.is_busy() {
            self.notify("Ya hay una operación en curso.", false);
            return;
        }
        let (tx, rx) = channel();
        let cancel = CancelToken::new();
        self.cancel = cancel.clone();
        let ctx = Ctx::with_events(Arc::clone(&self.logger), tx, cancel.clone());

        self.logger.info("app", detail.clone());
        self.running = Some(RunningTask {
            kind,
            receiver: rx,
            cancel: cancel.clone(),
            started: Instant::now(),
            accounts: BTreeMap::new(),
            summary: None,
            scan: None,
            detail: detail.clone(),
        });

        std::thread::spawn(move || {
            let task = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(ctx.clone())));
            match task {
                Ok(Ok(report)) => {
                    ctx.send(TaskEvent::Finished {
                        summary: Box::new(report),
                    });
                }
                Ok(Err(err)) => {
                    let mut report = RunReport::new("Operación");
                    report.cancelled = crate::cancel::is_cancelled(&err);
                    ctx.send(TaskEvent::Finished {
                        summary: Box::new(report),
                    });
                    // El error real se registra después del Finished para que la UI
                    // lo muestre en la consola incluso si cierra la tarea.
                    ctx.error("app", format!("La operación falló: {:#}", err));
                    ctx.logger.log(
                        LogLevel::Error,
                        "app",
                        "Revisá el detalle anterior: la operación no se completó.".to_string(),
                    );
                }
                Err(_) => {
                    ctx.error(
                        "app",
                        "La tarea terminó de forma inesperada (panic contenido). El progreso guardado se conserva.",
                    );
                    ctx.send(TaskEvent::Finished {
                        summary: Box::new(RunReport::new("Operación interrumpida")),
                    });
                }
            }
        });

        let _ = detail;
    }

    pub(crate) fn start_backup(&mut self) {
        if self.config.accounts.is_empty() {
            self.notify("Agregá al menos una cuenta antes de respaldar.", false);
            return;
        }
        let mut config = self.config.clone();
        config.output_dir = self.backup_state.output_dir.clone();
        config.concurrency_limit = self.backup_state.concurrency;
        config.zip_mode = self.backup_state.zip_mode;
        config.zip_compression = self.backup_state.zip_compression;
        config.export_format = self.backup_state.format;
        config.verify_after_backup = self.backup_state.verify;
        config.hash_verify = self.backup_state.hash_verify;
        config.cleanup_raw_after_zip = self.backup_state.cleanup_after_zip;
        config.skip_existing = self.backup_state.incremental;
        let secrets = self.secrets.clone();
        let dry_run = self.backup_state.dry_run;
        let force_full = self.backup_state.force_full;

        let detail = format!(
            "=== RESPALDO{}: {} cuenta(s) → {} ===",
            if dry_run { " (simulación)" } else { "" },
            config.accounts.len(),
            config.output_dir.display()
        );

        self.start_task(TaskKind::Backup, detail, move |ctx| {
            let mut opts = crate::backup::BackupOptions::from_config(&config);
            opts.dry_run = dry_run;
            opts.force_full = force_full;
            crate::backup::run(&config, &secrets, &ctx, &opts)
        });
    }

    pub(crate) fn start_restore(&mut self) {
        let Some(index) = self.restore_state.index.clone() else {
            self.notify("Analizá primero un ZIP, mbox o carpeta de origen.", false);
            return;
        };
        let Some(dest) = self.restore_state.dest_conn() else {
            self.notify("Completá host, correo y contraseña del destino.", false);
            return;
        };
        let dry_run = self.restore_state.dry_run;
        let dedup = self.restore_state.dedup;
        let prefix = self.restore_state.prefix.clone();
        let concurrency = self.restore_state.concurrency;
        let retry_attempts = self.config.retry_attempts;
        let base = self.config.retry_backoff_base_secs;
        let max = self.config.retry_backoff_max_secs;
        let verify = self.config.verify_after_backup;
        let source = self.restore_state.source.clone().unwrap_or_default();

        let detail = format!(
            "=== RESTAURACIÓN{}: {} → {} ===",
            if dry_run { " (simulación)" } else { "" },
            index.summary(),
            dest.describe()
        );

        self.start_task(TaskKind::Restore, detail, move |ctx| {
            let mut opts = crate::restore::RestoreOptions {
                source,
                dest,
                concurrency,
                skip_existing: dedup,
                dry_run,
                folder_prefix: Some(prefix.clone()).filter(|value| !value.trim().is_empty()),
                retry_attempts,
                backoff_base_secs: base,
                backoff_max_secs: max,
                verify,
            };
            opts.verify = verify && !dry_run;
            crate::restore::run(index, &opts, &ctx)
        });
    }

    pub(crate) fn start_migrate(&mut self) {
        let Some(source_email) = self.migrate_state.source_account.clone() else {
            self.notify("Elegí una cuenta origen.", false);
            return;
        };
        let Some(source_account) = self.config.account_by_email(&source_email).cloned() else {
            self.notify("La cuenta origen ya no está en la configuración.", false);
            return;
        };
        let source_password = match source_account.resolve_password(&self.secrets, false) {
            Ok(password) => password,
            Err(err) => {
                self.notify(format!("Falta la contraseña de origen: {}", err), false);
                return;
            }
        };
        let Some(dest) = self.migrate_state.dest_conn() else {
            self.notify("Completá el destino de la migración.", false);
            return;
        };

        let dry_run = self.migrate_state.dry_run;
        let dedup = self.migrate_state.dedup;
        let prefix = self.migrate_state.prefix.clone();
        let keep_local = self.migrate_state.keep_local_path();
        let keep_local_format = self.migrate_state.keep_local_format;
        let concurrency = self.migrate_state.concurrency;
        let timeout = self.config.timeout_secs;
        let retry_attempts = self.config.retry_attempts;
        let base = self.config.retry_backoff_base_secs;
        let max = self.config.retry_backoff_max_secs;
        let verify = self.config.verify_after_backup;
        let skip_all_mail = self.config.skip_all_mail;

        let detail = format!(
            "=== MIGRACIÓN{}: {} → {} ===",
            if dry_run { " (simulación)" } else { "" },
            source_email,
            dest.describe()
        );

        self.start_task(TaskKind::Migrate, detail, move |ctx| {
            let mut account = source_account;
            account.password = Some(source_password);
            let source_conn = crate::connect::ConnOptions::from_account(
                &account,
                &Secrets::default(),
            )?
            .with_timeout(timeout);
            let mut opts = crate::migrate::MigrateOptions::new(account, source_conn, dest);
            opts.concurrency = concurrency;
            opts.skip_existing = dedup;
            opts.dry_run = dry_run;
            opts.folder_prefix = Some(prefix).filter(|value| !value.trim().is_empty());
            opts.keep_local = keep_local.filter(|path| !path.as_os_str().is_empty());
            opts.keep_local_format = keep_local_format;
            opts.retry_attempts = retry_attempts;
            opts.backoff_base_secs = base;
            opts.backoff_max_secs = max;
            opts.verify = verify;
            opts.skip_all_mail = skip_all_mail;
            crate::migrate::run(&opts, &ctx)
        });
    }

    /// Analiza el origen de una restauración en segundo plano: el escaneo de un ZIP
    /// o de un mbox grande puede tardar minutos y no debe congelar la ventana.
    pub(crate) fn start_scan(&mut self, source: PathBuf) {
        if self.restore_state.scanning {
            self.notify("Ya hay un análisis en curso.", false);
            return;
        }
        let cancel = CancelToken::new();
        self.restore_state.scan_cancel = cancel.clone();
        self.restore_state.index = None;
        self.restore_state.scanning = true;
        self.restore_state.scan_status = "Analizando origen…".to_string();
        self.restore_state.scan_counters = None;

        // Dos canales: eventos (para la consola) y resultado (el índice).
        let (event_tx, event_rx) = channel();
        let (result_tx, result_rx) = channel();
        self.restore_state.scan_events = Some(event_rx);
        self.restore_state.scan_result = Some(result_rx);

        let ctx = Ctx::with_events(Arc::clone(&self.logger), event_tx, cancel);
        std::thread::spawn(move || {
            let result = crate::restore::build_index(&ctx, &source);
            let _ = result_tx.send(result.map_err(|err| format!("{:#}", err)));
        });
    }

    pub(crate) fn cancel_running(&mut self) {
        let Some(task) = self.running.as_ref() else {
            self.cancel.cancel();
            return;
        };
        task.cancel.cancel();
        self.logger.warn(
            "app",
            "Cancelación solicitada. La tarea se detendrá en el próximo punto seguro.",
        );
    }

    pub(crate) fn save_config(&mut self, path: &std::path::Path, include_passwords: bool) {
        match self.config.serialize_for(path, include_passwords) {
            Ok(content) => {
                if let Err(err) = std::fs::write(path, content) {
                    self.notify(format!("No se pudo escribir la configuración: {}", err), false);
                    return;
                }
                self.config_path = Some(path.to_path_buf());
                crate::fsutil::restrict_permissions(path);

                if include_passwords {
                    self.notify(format!("Configuración guardada en {}", path.display()), true);
                } else {
                    // Las contraseñas se mueven al archivo de secretos con permisos 0600.
                    for account in &self.config.accounts {
                        if let Some(password) = account.password.clone() {
                            if !password.is_empty() {
                                self.secrets.set(&account.email, &password);
                            }
                        }
                    }
                    match self.secrets.save(&self.secrets_path) {
                        Ok(()) => self.notify(
                            format!(
                                "Configuración guardada sin contraseñas (van a {})",
                                self.secrets_path.display()
                            ),
                            true,
                        ),
                        Err(err) => self.notify(
                            format!("Configuración guardada, pero fallaron los secretos: {}", err),
                            false,
                        ),
                    }
                }
            }
            Err(err) => self.notify(format!("Error de serialización: {}", err), false),
        }
    }

    fn apply_tab_actions(&mut self, actions: Vec<TabAction>) {
        for action in actions {
            match action {
                TabAction::None => {}
                TabAction::StartBackup => self.start_backup(),
                TabAction::StartRestore => self.start_restore(),
                TabAction::StartMigrate => self.start_migrate(),
                TabAction::ScanSource(path) => self.start_scan(path),
                TabAction::Probe(account) => self.start_probe(account),
                TabAction::Cancel => self.cancel_running(),
                TabAction::LoadConfig => {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Configuración", &["toml", "json"])
                        .pick_file()
                    {
                        match AppConfig::load_from_file(&path) {
                            Ok(config) => {
                                self.config = config;
                                self.config_path = Some(path.clone());
                                self.secrets = Secrets::load(&secrets_path(Some(&path)));
                                self.secrets_path = secrets_path(Some(&path));
                                self.backup_state.output_dir = self.config.output_dir.clone();
                                self.backup_state.concurrency = self.config.concurrency_limit;
                                self.backup_state.zip_mode = self.config.zip_mode;
                                self.backup_state.format = self.config.export_format;
                                self.notify(
                                    format!("Configuración cargada desde {}", path.display()),
                                    true,
                                );
                            }
                            Err(err) => {
                                self.notify(format!("Error cargando: {:#}", err), false);
                            }
                        }
                    }
                }
                TabAction::SaveConfig => {
                    let target = self
                        .config_path
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("config.toml"));
                    self.save_config(&target, true);
                }
                TabAction::SaveConfigAs => {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("TOML", &["toml"])
                        .add_filter("JSON", &["json"])
                        .set_file_name("config.toml")
                        .save_file()
                    {
                        self.save_config(&path, true);
                    }
                }
                TabAction::SaveConfigWithoutSecrets => {
                    let target = self
                        .config_path
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("config.toml"));
                    self.save_config(&target, false);
                }
                TabAction::NewConfig => {
                    self.config = AppConfig::default();
                    self.config_path = None;
                    self.backup_state.output_dir = self.config.output_dir.clone();
                    self.notify("Configuración nueva en memoria.", true);
                }
                TabAction::ExportLogs => {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_file_name("imap-backup-log.txt")
                        .save_file()
                    {
                        match self.logger.dump_to_file(&path) {
                            Ok(count) => {
                                self.notify(format!("{} línea(s) exportadas", count), true)
                            }
                            Err(err) => self.notify(format!("No se pudo escribir: {}", err), false),
                        }
                    }
                }
                TabAction::OpenFolder(path) => {
                    self.open_in_file_manager(&path);
                }
            }
        }
    }

    pub(crate) fn open_in_file_manager(&mut self, path: &std::path::Path) {
        #[cfg(windows)]
        let command = "explorer";
        #[cfg(target_os = "macos")]
        let command = "open";
        #[cfg(all(unix, not(target_os = "macos")))]
        let command = "xdg-open";

        match std::process::Command::new(command).arg(path).spawn() {
            Ok(_) => self.notify(format!("Abriendo {}", path.display()), true),
            Err(err) => self.notify(format!("No se pudo abrir la carpeta: {}", err), false),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pull_logs();
        self.drain_task_events();
        self.poll_probe();
        tabs::poll_scan(&mut self.restore_state, &self.logger);

        let busy = self.is_busy();
        if busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }

        let mut actions: Vec<TabAction> = Vec::new();

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(
                    RichText::new("📦 IMAP Backup & Migration Suite")
                        .strong()
                        .color(Color32::from_rgb(110, 190, 255)),
                );
                ui.label(
                    RichText::new(format!("v{}", crate::VERSION))
                        .color(Color32::GRAY)
                        .small(),
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some((message, when, ok)) = &self.notification {
                        if when.elapsed().as_secs() < 12 {
                            let color = if *ok {
                                Color32::from_rgb(120, 220, 140)
                            } else {
                                Color32::from_rgb(255, 120, 110)
                            };
                            ui.label(RichText::new(message).color(color).strong());
                        }
                    }
                    if busy {
                        ui.add(egui::Spinner::new());
                        let elapsed = self
                            .running
                            .as_ref()
                            .map(|task| task.started.elapsed().as_secs_f64())
                            .unwrap_or(0.0);
                        ui.label(
                            RichText::new(format!(
                                "{}… {:.0}s",
                                self.running
                                    .as_ref()
                                    .map(|task| task.kind.label())
                                    .unwrap_or("Trabajando"),
                                elapsed
                            ))
                            .color(Color32::from_rgb(255, 215, 0)),
                        );
                    }
                });
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                for (tab, label) in [
                    (Tab::Backup, "📥 Respaldar / Exportar"),
                    (Tab::Restore, "📤 Restaurar / Importar"),
                    (Tab::Migrate, "🔁 Migrar Servidor→Servidor"),
                ] {
                    if ui
                        .selectable_label(self.tab == tab, RichText::new(label).size(15.0).strong())
                        .clicked()
                    {
                        self.tab = tab;
                    }
                }

                ui.separator();
                if ui.button("📂 Cargar config").clicked() {
                    actions.push(TabAction::LoadConfig);
                }
                if ui.button("💾 Guardar").clicked() {
                    actions.push(TabAction::SaveConfig);
                }
                if ui.button("💾 Guardar como…").clicked() {
                    actions.push(TabAction::SaveConfigAs);
                }
                if ui
                    .button("🔒 Guardar sin contraseñas")
                    .on_hover_text("Mueve las contraseñas a secrets.toml con permisos 0600")
                    .clicked()
                {
                    actions.push(TabAction::SaveConfigWithoutSecrets);
                }
                if ui.button("🆕 Nueva").clicked() {
                    actions.push(TabAction::NewConfig);
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⏹ Cancelar").clicked() {
                        actions.push(TabAction::Cancel);
                    }
                    if busy {
                        ui.label(
                            RichText::new("Operación en curso")
                                .color(Color32::from_rgb(255, 215, 0)),
                        );
                    }
                });
            });

            ui.add_space(4.0);
            tabs::top_bar(self, ui, &mut actions);
            ui.add_space(6.0);
            ui.separator();
        });

        egui::TopBottomPanel::bottom("bottom")
            .min_height(240.0)
            .resizable(true)
            .show(ctx, |ui| {
                tabs::bottom_bar(self, ui, &mut actions);
            });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Backup => tabs::backup_tab(self, ui, &mut actions),
            Tab::Restore => tabs::restore_tab(self, ui, &mut actions),
            Tab::Migrate => tabs::migrate_tab(self, ui),
        });

        if self.show_report {
            tabs::report_window(self, ctx);
        }
        if self.account_modal.is_some() {
            tabs::account_modal(self, ctx);
        }

        self.apply_tab_actions(actions);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let state = PersistedUi {
            version: 2,
            tab: self.tab,
            config_path: self
                .config_path
                .as_ref()
                .map(|path| path.display().to_string()),
            auto_scroll: self.auto_scroll,
        };
        eframe::set_value(storage, eframe::APP_KEY, &state);
    }
}

/// Lanza la interfaz gráfica nativa.
pub fn run_native() -> eframe::Result<()> {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/icon.png")).ok();

    let mut viewport = ViewportBuilder::default()
        .with_title("IMAP Backup & Migration Suite")
        .with_inner_size(Vec2::new(1080.0, 760.0))
        .with_min_inner_size(Vec2::new(820.0, 560.0))
        .with_resizable(true);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        default_theme: eframe::Theme::Dark,
        persist_window: true,
        ..Default::default()
    };

    eframe::run_native(
        "IMAP Backup & Migration Suite",
        options,
        Box::new(|cc| Box::new(App::new(cc))),
    )
}

/// Acciones que la UI pide y que se ejecutan al final del frame (evita préstamos
/// mutables cruzados entre pestañas y el estado global).
#[derive(Debug, Clone)]
pub enum TabAction {
    None,
    StartBackup,
    StartRestore,
    StartMigrate,
    ScanSource(PathBuf),
    Probe(String),
    Cancel,
    LoadConfig,
    SaveConfig,
    SaveConfigAs,
    SaveConfigWithoutSecrets,
    NewConfig,
    ExportLogs,
    OpenFolder(PathBuf),
}
