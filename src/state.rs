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

use crate::config::AccountConfig;
use crate::imap_client::AccountStats;
use chrono::Local;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warning,
    Error,
    Success,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: LogLevel,
    pub message: String,
}

impl LogEntry {
    pub fn new(level: LogLevel, message: impl Into<String>) -> Self {
        Self {
            timestamp: Local::now().format("%H:%M:%S").to_string(),
            level,
            message: message.into(),
        }
    }

    pub fn info(message: impl Into<String>) -> Self {
        Self::new(LogLevel::Info, message)
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(LogLevel::Warning, message)
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(LogLevel::Error, message)
    }

    pub fn success(message: impl Into<String>) -> Self {
        Self::new(LogLevel::Success, message)
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct OverallSummary {
    pub total_accounts: usize,
    pub total_downloaded: usize,
    pub total_skipped: usize,
    pub total_bytes: u64,
    pub total_errors: usize,
    pub elapsed_seconds: f64,
}

#[derive(Debug, Clone)]
pub enum BackupEvent {
    Log(LogEntry),
    AccountStarted {
        account: String,
    },
    FolderProgress {
        account: String,
        folder: String,
        current: u64,
        total: u64,
    },
    AccountFinished {
        account: String,
        success: bool,
        stats: AccountStats,
    },
    OverallFinished(OverallSummary),
}

#[derive(Debug, Clone, Default)]
pub struct AccountProgressState {
    pub status: String,
    pub current_folder: String,
    pub current_msgs: u64,
    pub total_msgs: u64,
    pub is_finished: bool,
    pub has_error: bool,
}

#[derive(Debug, Clone)]
pub struct AccountEditModal {
    pub is_open: bool,
    pub edit_index: Option<usize>, // None = Creando nueva cuenta, Some(idx) = Editando
    pub email: String,
    pub password: String,
    pub host: String,
    pub port: String,
    pub tls: bool,
    pub label: String,
    pub exclude_folders: String,
    pub error_msg: Option<String>,
}

impl Default for AccountEditModal {
    fn default() -> Self {
        Self {
            is_open: false,
            edit_index: None,
            email: String::new(),
            password: String::new(),
            host: String::new(),
            port: "993".to_string(),
            tls: true,
            label: String::new(),
            exclude_folders: "Spam, Trash, Papelera".to_string(),
            error_msg: None,
        }
    }
}

impl AccountEditModal {
    pub fn open_for_new() -> Self {
        Self {
            is_open: true,
            edit_index: None,
            ..Default::default()
        }
    }

    pub fn open_for_edit(index: usize, acc: &AccountConfig) -> Self {
        Self {
            is_open: true,
            edit_index: Some(index),
            email: acc.email.clone(),
            password: acc.password.clone(),
            host: acc.host.clone(),
            port: acc.port.to_string(),
            tls: acc.tls,
            label: acc.label.clone().unwrap_or_default(),
            exclude_folders: acc.exclude_folders.join(", "),
            error_msg: None,
        }
    }

    pub fn to_account_config(&self) -> Result<AccountConfig, String> {
        if self.email.trim().is_empty() {
            return Err("El correo electrónico no puede estar vacío.".to_string());
        }
        if self.host.trim().is_empty() {
            return Err("El host del servidor IMAP no puede estar vacío.".to_string());
        }
        if self.password.is_empty() {
            return Err("La contraseña no puede estar vacía.".to_string());
        }

        let port = self
            .port
            .trim()
            .parse::<u16>()
            .map_err(|_| "El puerto debe ser un número válido (ej. 993).".to_string())?;

        let exclude_folders = self
            .exclude_folders
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let label = if self.label.trim().is_empty() {
            None
        } else {
            Some(self.label.trim().to_string())
        };

        Ok(AccountConfig {
            email: self.email.trim().to_string(),
            password: self.password.clone(),
            host: self.host.trim().to_string(),
            port,
            tls: self.tls,
            label,
            exclude_folders,
            include_only_folders: None,
        })
    }
}
