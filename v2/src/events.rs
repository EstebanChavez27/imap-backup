// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Registro de actividad y canal de eventos hacia la interfaz.
//!
//! El logger es un buffer circular con capacidad fija: un backup de 100.000 correos
//! no puede hacer crecer la memoria (ni el costo de repintado de la UI) de forma
//! ilimitada. Además se puede duplicar a un archivo de log, lo que hace utilizables
//! las corridas programadas en modo headless.

use crate::cancel::CancelToken;
use crate::report::{AccountReport, RunReport};
use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug,
    Info,
    Success,
    Warning,
    Error,
}

impl LogLevel {
    pub fn badge(self) -> &'static str {
        match self {
            LogLevel::Debug => "[DBG]",
            LogLevel::Info => "[INFO]",
            LogLevel::Success => "[ OK ]",
            LogLevel::Warning => "[WARN]",
            LogLevel::Error => "[ERROR]",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Success => "OK",
            LogLevel::Warning => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub ts: String,
    pub level: LogLevel,
    pub scope: String,
    pub message: String,
}

impl LogEntry {
    pub fn formatted(&self) -> String {
        if self.scope.is_empty() {
            format!("[{}] {} {}", self.ts, self.level.badge(), self.message)
        } else {
            format!(
                "[{}] {} [{}] {}",
                self.ts,
                self.level.badge(),
                self.scope,
                self.message
            )
        }
    }
}

struct LogInner {
    entries: VecDeque<LogEntry>,
    capacity: usize,
    total: u64,
    file: Option<std::fs::File>,
    min_level: LogLevel,
    console: bool,
}

pub struct Logger {
    inner: Mutex<LogInner>,
}

impl Default for Logger {
    fn default() -> Self {
        Self::with_capacity(4000)
    }
}

impl Logger {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(LogInner {
                entries: VecDeque::with_capacity(capacity.min(1024)),
                capacity: capacity.max(100),
                total: 0,
                file: None,
                min_level: LogLevel::Debug,
                console: false,
            }),
        }
    }

    /// Duplica cada mensaje a la salida estándar. Es lo que hace usable el modo
    /// headless: en la v1 no había forma de ver el progreso desde una consola.
    pub fn set_console(&self, enabled: bool) {
        self.inner.lock().unwrap().console = enabled;
    }

    /// Duplica todos los mensajes (desde el nivel `min_level`) a un archivo de log.
    pub fn set_log_file(&self, path: &Path, min_level: LogLevel) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let mut inner = self.inner.lock().unwrap();
        inner.file = Some(file);
        inner.min_level = min_level;
        Ok(())
    }

    pub fn log(&self, level: LogLevel, scope: &str, message: impl Into<String>) {
        let entry = LogEntry {
            ts: chrono::Local::now().format("%H:%M:%S").to_string(),
            level,
            scope: scope.to_string(),
            message: message.into(),
        };
        let mut inner = self.inner.lock().unwrap();
        let min_level = inner.min_level;
        if level >= min_level {
            if let Some(file) = inner.file.as_mut() {
                let line = format!(
                    "{} {}\n",
                    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S"),
                    entry.formatted(),
                );
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
            }
        }
        let console = inner.console;
        if console {
            println!("{}", entry.formatted());
        }
        inner.entries.push_back(entry);
        inner.total += 1;
        while inner.entries.len() > inner.capacity {
            inner.entries.pop_front();
        }
    }

    pub fn debug(&self, scope: &str, message: impl Into<String>) {
        self.log(LogLevel::Debug, scope, message);
    }

    pub fn info(&self, scope: &str, message: impl Into<String>) {
        self.log(LogLevel::Info, scope, message);
    }

    pub fn success(&self, scope: &str, message: impl Into<String>) {
        self.log(LogLevel::Success, scope, message);
    }

    pub fn warn(&self, scope: &str, message: impl Into<String>) {
        self.log(LogLevel::Warning, scope, message);
    }

    pub fn error(&self, scope: &str, message: impl Into<String>) {
        self.log(LogLevel::Error, scope, message);
    }

    /// Cantidad total de entradas emitidas (contador monótono, incluye las descartadas).
    pub fn total(&self) -> u64 {
        self.inner.lock().unwrap().total
    }

    pub fn capacity(&self) -> usize {
        self.inner.lock().unwrap().capacity
    }

    pub fn set_capacity(&self, capacity: usize) {
        let mut inner = self.inner.lock().unwrap();
        inner.capacity = capacity.max(100);
        while inner.entries.len() > inner.capacity {
            inner.entries.pop_front();
        }
    }

    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.inner.lock().unwrap().entries.iter().cloned().collect()
    }

    /// Devuelve las entradas nuevas a partir de un contador y el nuevo cursor.
    /// Permite a la UI copiar solo lo nuevo en vez de clonar todo el buffer.
    pub fn snapshot_from(&self, cursor: &mut u64) -> Vec<LogEntry> {
        let inner = self.inner.lock().unwrap();
        let total = inner.total;
        let buffered = inner.entries.len() as u64;
        let first_seq = total - buffered;
        if *cursor < first_seq {
            // El cursor quedó por detrás del buffer circular: se devuelve todo.
            *cursor = total;
            return inner.entries.iter().cloned().collect();
        }
        let skip = (*cursor - first_seq) as usize;
        let out: Vec<LogEntry> = inner.entries.iter().skip(skip).cloned().collect();
        *cursor = total;
        out
    }

    pub fn clear(&self) {
        self.inner.lock().unwrap().entries.clear();
    }

    /// Vuelca todo el buffer a un archivo (usado por "Exportar logs" en la UI).
    pub fn dump_to_file(&self, path: &Path) -> std::io::Result<usize> {
        let snapshot = self.snapshot();
        let mut out = String::new();
        for entry in &snapshot {
            out.push_str(&entry.formatted());
            out.push('\n');
        }
        std::fs::write(path, out)?;
        Ok(snapshot.len())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Backup,
    Restore,
    Migrate,
    Scan,
}

impl TaskKind {
    pub fn label(self) -> &'static str {
        match self {
            TaskKind::Backup => "Respaldo",
            TaskKind::Restore => "Restauración",
            TaskKind::Migrate => "Migración",
            TaskKind::Scan => "Análisis",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AccountRunState {
    Queued,
    Running {
        folder: String,
        current: u64,
        total: u64,
        bytes: u64,
    },
    Finished(Box<AccountReport>),
    Failed(String),
}

#[derive(Debug, Clone)]
pub enum TaskEvent {
    Started {
        kind: TaskKind,
        detail: String,
    },
    Account {
        account: String,
        state: AccountRunState,
    },
    /// Mensajes analizados/localizados durante un escaneo previo (ZIP o carpeta).
    ScanProgress {
        detail: String,
        messages: u64,
        folders: u64,
        bytes: u64,
    },
    Finished {
        summary: Box<RunReport>,
    },
}

/// Contexto compartido que reciben todos los motores (backup, restore, migrate):
/// logger, canal de eventos opcional (None en modo headless) y token de cancelación.
#[derive(Clone)]
pub struct Ctx {
    pub logger: Arc<Logger>,
    pub events: Option<Sender<TaskEvent>>,
    pub cancel: CancelToken,
}

impl Ctx {
    pub fn headless(logger: Arc<Logger>, cancel: CancelToken) -> Self {
        Self {
            logger,
            events: None,
            cancel,
        }
    }

    pub fn with_events(logger: Arc<Logger>, events: Sender<TaskEvent>, cancel: CancelToken) -> Self {
        Self {
            logger,
            events: Some(events),
            cancel,
        }
    }

    pub fn log(&self, level: LogLevel, scope: &str, message: impl Into<String>) {
        self.logger.log(level, scope, message);
    }

    pub fn debug(&self, scope: &str, message: impl Into<String>) {
        self.log(LogLevel::Debug, scope, message);
    }

    pub fn info(&self, scope: &str, message: impl Into<String>) {
        self.log(LogLevel::Info, scope, message);
    }

    pub fn success(&self, scope: &str, message: impl Into<String>) {
        self.log(LogLevel::Success, scope, message);
    }

    pub fn warn(&self, scope: &str, message: impl Into<String>) {
        self.log(LogLevel::Warning, scope, message);
    }

    pub fn error(&self, scope: &str, message: impl Into<String>) {
        self.log(LogLevel::Error, scope, message);
    }

    pub fn send(&self, event: TaskEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn check_cancel(&self) -> anyhow::Result<()> {
        crate::cancel::check(&self.cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_discards_oldest_entries() {
        let logger = Logger::with_capacity(100);
        for i in 0..500 {
            logger.log(LogLevel::Info, "test", format!("m{}", i));
        }
        assert_eq!(logger.total(), 500);
        assert_eq!(logger.snapshot().len(), 100);
        assert_eq!(logger.snapshot()[0].message, "m400");
    }

    #[test]
    fn incremental_cursor_returns_only_new_entries() {
        let logger = Logger::with_capacity(1000);
        logger.log(LogLevel::Info, "t", "uno");
        let mut cursor = 0u64;
        let first = logger.snapshot_from(&mut cursor);
        assert_eq!(first.len(), 1);
        let empty = logger.snapshot_from(&mut cursor);
        assert!(empty.is_empty());
        logger.log(LogLevel::Warning, "t", "dos");
        let second = logger.snapshot_from(&mut cursor);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].level, LogLevel::Warning);
    }

    #[test]
    fn cursor_behind_buffer_returns_everything() {
        let logger = Logger::with_capacity(100);
        let mut cursor = 0u64;
        for i in 0..300 {
            logger.log(LogLevel::Info, "t", format!("m{}", i));
        }
        let all = logger.snapshot_from(&mut cursor);
        assert_eq!(all.len(), 100);
        assert_eq!(cursor, 300);
    }

    #[test]
    fn log_file_receives_entries_at_or_above_min_level() {
        let dir = std::env::temp_dir().join(format!("imapb-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("run.log");
        let logger = Logger::with_capacity(100);
        logger.set_log_file(&path, LogLevel::Info).unwrap();
        logger.log(LogLevel::Debug, "s", "no debe aparecer");
        logger.log(LogLevel::Error, "s", "si debe aparecer");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("no debe aparecer"));
        assert!(content.contains("si debe aparecer"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
