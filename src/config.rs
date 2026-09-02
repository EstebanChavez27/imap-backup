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

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ZipMode {
    #[default]
    #[serde(rename = "PerAccount")]
    PerAccount,
    #[serde(rename = "Consolidated")]
    Consolidated,
    #[serde(rename = "None")]
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub email: String,
    pub password: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_true")]
    pub tls: bool,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub exclude_folders: Vec<String>,
    #[serde(default)]
    pub include_only_folders: Option<Vec<String>>,
}

impl AccountConfig {
    pub fn get_domain_or_label(&self) -> String {
        if let Some(ref l) = self.label {
            if !l.trim().is_empty() {
                return l.trim().to_string();
            }
        }
        if let Some(domain) = self.email.split('@').nth(1) {
            domain.to_string()
        } else {
            "default".to_string()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_output_dir")]
    pub output_dir: PathBuf,
    #[serde(default = "default_concurrency")]
    pub concurrency_limit: usize,
    #[serde(default)]
    pub zip_mode: ZipMode,
    #[serde(default)]
    pub cleanup_raw_after_zip: bool,
    #[serde(default = "default_retry_attempts")]
    pub retry_attempts: u32,
    #[serde(default = "default_retry_delay_secs")]
    pub retry_delay_secs: u64,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_true")]
    pub skip_existing: bool,
    pub accounts: Vec<AccountConfig>,
}

fn default_port() -> u16 {
    993
}

fn default_true() -> bool {
    true
}

fn default_output_dir() -> PathBuf {
    PathBuf::from("backups")
}

fn default_concurrency() -> usize {
    3
}

fn default_retry_attempts() -> u32 {
    3
}

fn default_retry_delay_secs() -> u64 {
    5
}

fn default_timeout_secs() -> u64 {
    45
}

impl AppConfig {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_ref = path.as_ref();
        if !path_ref.exists() {
            bail!("El archivo de configuración no existe: {}", path_ref.display());
        }

        let content = fs::read_to_string(path_ref)
            .with_context(|| format!("Error leyendo el archivo: {}", path_ref.display()))?;

        let extension = path_ref
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();

        let config: AppConfig = match extension.as_str() {
            "json" => serde_json::from_str(&content)
                .with_context(|| "Error parseando formato JSON de configuración")?,
            "toml" => toml::from_str(&content)
                .with_context(|| "Error parseando formato TOML de configuración")?,
            _ => {
                // Intento automático: primero TOML, luego JSON
                if let Ok(c) = toml::from_str(&content) {
                    c
                } else {
                    serde_json::from_str(&content)
                        .with_context(|| "No se pudo interpretar el archivo como TOML ni como JSON")?
                }
            }
        };

        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.accounts.is_empty() {
            bail!("La lista de cuentas en la configuración está vacía.");
        }

        for (idx, acc) in self.accounts.iter().enumerate() {
            if acc.email.trim().is_empty() {
                bail!("La cuenta en posición #{} no tiene definido un email válido.", idx + 1);
            }
            if acc.host.trim().is_empty() {
                bail!("La cuenta '{}' no tiene configurado un host IMAP.", acc.email);
            }
            if acc.password.is_empty() {
                bail!("La cuenta '{}' tiene una contraseña vacía.", acc.email);
            }
        }

        Ok(())
    }
}
