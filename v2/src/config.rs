// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Configuración, credenciales y descubrimiento de archivos.
//!
//! Compatibilidad con la v1: se aceptan los mismos nombres de campo
//! (`output_dir`, `concurrency_limit`, `zip_mode`, `retry_delay_secs`,
//! `cleanup_raw_after_zip`, `exclude_folders`, …) y `tls` puede seguir siendo un
//! booleano. En la v1 ese campo se ignoraba por completo; acá se traduce a un modo
//! de conexión real (`Implicit`/`StartTls`/`Plain`).

use crate::folders::sanitize_component;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Enumeraciones de configuración
// ---------------------------------------------------------------------------

/// Modo de cifrado de la conexión IMAP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsMode {
    /// TLS directo (puerto 993).
    #[default]
    Implicit,
    /// Conexión en claro y posterior `STARTTLS` (habitualmente puerto 143).
    StartTls,
    /// Sin cifrado. Solo para servidores internos/de laboratorio.
    Plain,
}

impl TlsMode {
    pub fn label(self) -> &'static str {
        match self {
            TlsMode::Implicit => "SSL/TLS directo (993)",
            TlsMode::StartTls => "STARTTLS (143)",
            TlsMode::Plain => "Sin cifrado (inseguro)",
        }
    }

    fn from_str_lenient(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "implicit" | "tls" | "ssl" | "smtps" | "direct" | "true" | "yes" | "1" => {
                Some(TlsMode::Implicit)
            }
            "starttls" | "start_tls" | "start-tls" | "upgrade" => Some(TlsMode::StartTls),
            "plain" | "plaintext" | "none" | "insecure" | "false" | "no" | "0" => {
                Some(TlsMode::Plain)
            }
            _ => None,
        }
    }
}

impl Serialize for TlsMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = match self {
            TlsMode::Implicit => "Implicit",
            TlsMode::StartTls => "StartTls",
            TlsMode::Plain => "Plain",
        };
        serializer.serialize_str(value)
    }
}

impl<'de> Deserialize<'de> for TlsMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Bool(bool),
            Text(String),
        }

        let repr = Repr::deserialize(deserializer)?;
        let mode = match repr {
            Repr::Bool(true) => TlsMode::Implicit,
            Repr::Bool(false) => TlsMode::Plain,
            Repr::Text(text) => TlsMode::from_str_lenient(&text)
                .ok_or_else(|| serde::de::Error::custom(format!("modo TLS desconocido: {}", text)))?,
        };
        Ok(mode)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ZipMode {
    #[default]
    PerAccount,
    Consolidated,
    None,
}

impl ZipMode {
    pub fn label(self) -> &'static str {
        match self {
            ZipMode::PerAccount => "Un ZIP por cuenta",
            ZipMode::Consolidated => "ZIP maestro consolidado",
            ZipMode::None => "Sin ZIP (carpetas sueltas)",
        }
    }
}

macro_rules! named_enum_serde {
    ($ty:ty, $($variant:ident => $name:literal),+ $(,)?) => {
        impl Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                $(if matches!(self, <$ty>::$variant) { return serializer.serialize_str($name); })+
                unreachable!()
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                let lowered = text.trim().to_lowercase().replace(['-', '_'], "");
                $(
                    if lowered == $name.to_lowercase().replace(['-', '_'], "") {
                        return Ok(<$ty>::$variant);
                    }
                )+
                Err(serde::de::Error::custom(format!(
                    "valor no reconocido '{}'",
                    text
                )))
            }
        }
    };
}

named_enum_serde!(ZipMode,
    PerAccount => "PerAccount",
    Consolidated => "Consolidated",
    None => "None",
);

/// Formato de salida del backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportFormat {
    /// Un archivo `.eml` por mensaje (formato histórico, compatible con la v1).
    #[default]
    Eml,
    /// Un archivo `.mbox` por carpeta (mboxrd), importable en Thunderbird.
    Mbox,
    /// Un directorio Maildir por carpeta, con flags en el nombre del archivo.
    Maildir,
}

named_enum_serde!(ExportFormat,
    Eml => "Eml",
    Mbox => "Mbox",
    Maildir => "Maildir",
);

impl ExportFormat {
    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Eml => ".eml por mensaje",
            ExportFormat::Mbox => ".mbox por carpeta",
            ExportFormat::Maildir => "Maildir por carpeta",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ZipCompression {
    #[default]
    Deflate,
    Store,
}

named_enum_serde!(ZipCompression,
    Deflate => "Deflate",
    Store => "Store",
);

impl ZipCompression {
    pub fn label(self) -> &'static str {
        match self {
            ZipCompression::Deflate => "Comprimir (deflate)",
            ZipCompression::Store => "Sin comprimir (más rápido)",
        }
    }
}

// ---------------------------------------------------------------------------
// Cuentas
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub email: String,
    /// Contraseña en texto plano (compatible con la v1). Opcional si se usa
    /// `password_env` o el archivo de secretos.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Nombre de la variable de entorno que contiene la contraseña. Es la forma
    /// recomendada para automatizar backups sin dejar secretos en el archivo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_env: Option<String>,
    /// Nombre de la variable de entorno con un token OAuth2 (XOAUTH2). Necesario
    /// para Microsoft 365 y Google Workspace, donde la autenticación básica ya no
    /// está disponible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_token_env: Option<String>,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub tls: TlsMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub exclude_folders: Vec<String>,
    #[serde(default)]
    pub include_only_folders: Option<Vec<String>>,
    /// Acepta certificados TLS inválidos. Solo para servidores propios con
    /// certificados autofirmados.
    #[serde(default)]
    pub insecure_skip_tls_verify: bool,
}

fn default_port() -> u16 {
    993
}

impl AccountConfig {
    pub fn new(email: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            email: email.into(),
            password: None,
            password_env: None,
            oauth_token_env: None,
            host: host.into(),
            port: 993,
            tls: TlsMode::Implicit,
            label: None,
            exclude_folders: vec!["Spam".into(), "Trash".into(), "Papelera".into()],
            include_only_folders: None,
            insecure_skip_tls_verify: false,
        }
    }

    pub fn get_domain_or_label(&self) -> String {
        if let Some(label) = &self.label {
            if !label.trim().is_empty() {
                return label.trim().to_string();
            }
        }
        self.email
            .split('@')
            .nth(1)
            .unwrap_or("default")
            .to_string()
    }

    pub fn effective_port(&self) -> u16 {
        if self.port != 0 {
            self.port
        } else {
            match self.tls {
                TlsMode::Implicit => 993,
                TlsMode::StartTls | TlsMode::Plain => 143,
            }
        }
    }

    /// Resuelve la contraseña según prioridad: campo directo → variable de entorno
    /// → archivo de secretos → prompt interactivo (solo en modo headless).
    pub fn resolve_password(
        &self,
        secrets: &Secrets,
        allow_prompt: bool,
    ) -> Result<String> {
        if let Some(pwd) = &self.password {
            if !pwd.is_empty() {
                return Ok(clean_secret(pwd));
            }
        }
        if let Some(var) = &self.password_env {
            if let Ok(value) = std::env::var(var) {
                if !value.is_empty() {
                    return Ok(clean_secret(&value));
                }
            }
            if allow_prompt {
                bail!(
                    "La variable de entorno '{}' no está definida para la cuenta {}",
                    var,
                    self.email
                );
            }
        }
        if let Some(pwd) = secrets.passwords.get(&self.email) {
            if !pwd.is_empty() {
                return Ok(clean_secret(pwd));
            }
        }
        if allow_prompt {
            let pwd = prompt_secret(&format!("Contraseña IMAP para {}: ", self.email))?;
            if pwd.is_empty() {
                bail!("Contraseña vacía para {}", self.email);
            }
            return Ok(clean_secret(&pwd));
        }
        bail!(
            "No hay contraseña para {}: definí 'password', 'password_env' o el archivo de secretos.",
            self.email
        )
    }

    pub fn resolve_oauth_token(&self) -> Option<String> {
        self.oauth_token_env
            .as_ref()
            .and_then(|var| std::env::var(var).ok())
            .filter(|v| !v.trim().is_empty())
            .map(|v| clean_secret(&v))
    }
}

/// Quita CR/LF (defensa contra inyección de comandos IMAP vía configuración).
pub fn clean_secret(value: &str) -> String {
    value.replace(['\r', '\n'], "")
}

/// Evita que un proceso sin terminal (cron, CI, `ssh` sin tty) quede esperando una
/// contraseña que nunca va a llegar: mejor un error que explique cómo pasarla.
fn refuse_non_interactive() -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "Hace falta una contraseña y la entrada estándar no es una terminal.\n\
             Definí `password_env` (o `password`) en la configuración, usá el archivo \
             de secretos, o pasá --password-env/--dest-password-env VAR."
        );
    }
    Ok(())
}

/// Lee un secreto por terminal sin mostrarlo en pantalla (en Unix desactiva el eco
/// con `termios`; en otras plataformas avisa que la entrada será visible).
#[cfg(unix)]
pub fn prompt_secret(prompt: &str) -> Result<String> {
    use std::io::{BufRead, IsTerminal, Write};
    refuse_non_interactive()?;

    print!("{}", prompt);
    std::io::stdout().flush().ok();

    // SAFETY: se opera sobre el descriptor de entrada estándar con la estructura
    // termios propia; se restaura siempre el modo original antes de retornar.
    let fd = libc::STDIN_FILENO;
    let mut original: libc::termios = unsafe { std::mem::zeroed() };
    let have_termios = unsafe { libc::tcgetattr(fd, &mut original) } == 0;
    if have_termios {
        let mut hidden = original;
        hidden.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &hidden) };
    }

    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);

    if have_termios {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &original) };
        println!();
    }
    read.with_context(|| "No se pudo leer la contraseña desde la terminal")?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(not(unix))]
pub fn prompt_secret(prompt: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    refuse_non_interactive()?;
    eprintln!(
        "AVISO: en esta plataforma la contraseña se mostrará mientras se escribe. "
    );
    print!("{}", prompt);
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .with_context(|| "No se pudo leer la contraseña desde la terminal")?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Hooks {
    /// Comando que se ejecuta al terminar un backup (p. ej. `rclone copy ...`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_backup_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_restore_command: Option<String>,
    /// Webhook HTTP(S) al que se envía el resumen cuando hay errores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_webhook_url: Option<String>,
    /// Envía también el webhook cuando la corrida termina sin errores.
    #[serde(default)]
    pub webhook_on_success: bool,
}

// ---------------------------------------------------------------------------
// Configuración global
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_output_dir")]
    pub output_dir: PathBuf,
    /// Cuentas procesadas en paralelo durante el backup.
    #[serde(default = "default_concurrency")]
    pub concurrency_limit: usize,
    /// Carpetas subidas en paralelo durante restauración/migración.
    #[serde(default = "default_concurrency")]
    pub restore_concurrency: usize,
    #[serde(default)]
    pub zip_mode: ZipMode,
    #[serde(default)]
    pub zip_compression: ZipCompression,
    #[serde(default)]
    pub cleanup_raw_after_zip: bool,
    #[serde(default = "default_retry_attempts")]
    pub retry_attempts: u32,
    #[serde(default = "default_retry_base", alias = "retry_delay_secs")]
    pub retry_backoff_base_secs: u64,
    #[serde(default = "default_retry_max")]
    pub retry_backoff_max_secs: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_true")]
    pub skip_existing: bool,
    /// Verifica al final que el conteo local coincida con el del servidor.
    #[serde(default = "default_true")]
    pub verify_after_backup: bool,
    /// Calcula hash SHA-256 de cada mensaje descargado (detección de corrupción local).
    #[serde(default)]
    pub hash_verify: bool,
    #[serde(default)]
    pub export_format: ExportFormat,
    /// Con `export_format` distinto de `.eml`, además deja los `.eml` en disco.
    #[serde(default)]
    pub keep_eml_alongside: bool,
    /// Omite carpetas virtuales `\All` (Gmail): sus mensajes ya están en las demás.
    #[serde(default = "default_true")]
    pub skip_all_mail: bool,
    /// Presupuesto máximo de bytes por lote de descarga (memoria acotada).
    #[serde(default = "default_chunk_bytes")]
    pub chunk_bytes: u64,
    #[serde(default = "default_max_chunk_messages")]
    pub max_chunk_messages: usize,
    /// Evita volver a subir mensajes que ya existen en el destino (por Message-ID).
    #[serde(default = "default_true")]
    pub dedup_on_restore: bool,
    #[serde(default)]
    pub hooks: Hooks,
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
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
fn default_retry_base() -> u64 {
    3
}
fn default_retry_max() -> u64 {
    60
}
fn default_timeout() -> u64 {
    45
}
fn default_true() -> bool {
    true
}
fn default_chunk_bytes() -> u64 {
    8 * 1024 * 1024
}
fn default_max_chunk_messages() -> usize {
    200
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            output_dir: default_output_dir(),
            concurrency_limit: default_concurrency(),
            restore_concurrency: default_concurrency(),
            zip_mode: ZipMode::default(),
            zip_compression: ZipCompression::default(),
            cleanup_raw_after_zip: false,
            retry_attempts: default_retry_attempts(),
            retry_backoff_base_secs: default_retry_base(),
            retry_backoff_max_secs: default_retry_max(),
            timeout_secs: default_timeout(),
            skip_existing: true,
            verify_after_backup: true,
            hash_verify: false,
            export_format: ExportFormat::default(),
            keep_eml_alongside: false,
            skip_all_mail: true,
            chunk_bytes: default_chunk_bytes(),
            max_chunk_messages: default_max_chunk_messages(),
            dedup_on_restore: true,
            hooks: Hooks::default(),
            accounts: Vec::new(),
        }
    }
}

impl AppConfig {
    /// Carga y valida la configuración deduciendo el formato por extensión
    /// (o probando TOML y luego JSON si no hay extensión reconocible).
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            bail!("El archivo de configuración no existe: {}", path.display());
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Error leyendo el archivo: {}", path.display()))?;
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();

        let mut config: AppConfig = match extension.as_str() {
            "json" => serde_json::from_str(&content)
                .with_context(|| "Error parseando el formato JSON de configuración")?,
            "toml" => toml::from_str(&content)
                .with_context(|| "Error parseando el formato TOML de configuración")?,
            _ => match toml::from_str(&content) {
                Ok(cfg) => cfg,
                Err(toml_err) => serde_json::from_str(&content).map_err(|json_err| {
                    anyhow::anyhow!(
                        "No se pudo interpretar como TOML ({}) ni como JSON ({})",
                        toml_err,
                        json_err
                    )
                })?,
            },
        };

        for account in &mut config.accounts {
            account.email = clean_secret(account.email.trim());
            account.host = clean_secret(account.host.trim());
            if let Some(pwd) = account.password.as_mut() {
                *pwd = clean_secret(pwd);
            }
        }

        config.validate()?;
        Ok(config)
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Serializa para guardar. Con `include_passwords = false` las contraseñas se
    /// omiten (van al archivo de secretos con permisos restringidos).
    pub fn serialize_for(&self, path: &Path, include_passwords: bool) -> Result<String> {
        let mut copy = self.clone();
        if !include_passwords {
            for account in &mut copy.accounts {
                account.password = None;
            }
        }
        match path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("toml")
            .to_lowercase()
            .as_str()
        {
            "json" => copy.to_json(),
            _ => copy.to_toml(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();

        if self.accounts.is_empty() {
            problems.push("La lista de cuentas está vacía.".to_string());
        }
        if self.concurrency_limit == 0 || self.concurrency_limit > 32 {
            problems.push("concurrency_limit debe estar entre 1 y 32.".to_string());
        }
        if self.restore_concurrency == 0 || self.restore_concurrency > 32 {
            problems.push("restore_concurrency debe estar entre 1 y 32.".to_string());
        }
        if self.timeout_secs == 0 {
            problems.push("timeout_secs debe ser mayor que 0.".to_string());
        }
        if self.chunk_bytes < 64 * 1024 {
            problems.push("chunk_bytes es demasiado pequeño (mínimo 64 KB).".to_string());
        }
        if self.max_chunk_messages == 0 {
            problems.push("max_chunk_messages debe ser mayor que 0.".to_string());
        }

        let mut seen = std::collections::HashSet::new();
        for (idx, account) in self.accounts.iter().enumerate() {
            let position = idx + 1;
            if account.email.trim().is_empty() {
                problems.push(format!("La cuenta #{} no tiene email.", position));
            }
            if account.host.trim().is_empty() {
                problems.push(format!(
                    "La cuenta '{}' no tiene host IMAP configurado.",
                    account.email
                ));
            }
            if !seen.insert(account.email.to_lowercase()) {
                problems.push(format!(
                    "La cuenta '{}' está duplicada en la configuración.",
                    account.email
                ));
            }
            let has_secret = account
                .password
                .as_ref()
                .is_some_and(|p| !p.is_empty())
                || account.password_env.is_some()
                || account.oauth_token_env.is_some();
            if !has_secret {
                problems.push(format!(
                    "La cuenta '{}' no tiene contraseña, password_env ni oauth_token_env (puede resolverse con el archivo de secretos).",
                    account.email
                ));
            }
            if let Some(list) = &account.include_only_folders {
                if list.is_empty() {
                    problems.push(format!(
                        "La cuenta '{}' define include_only_folders vacío; quitalo o agregá carpetas.",
                        account.email
                    ));
                }
            }
        }

        if problems.is_empty() {
            Ok(())
        } else {
            bail!("Configuración inválida:\n  - {}", problems.join("\n  - "))
        }
    }

    /// Advertencias no bloqueantes que conviene mostrar en la UI/CLI.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.accounts.iter().any(|a| a.tls == TlsMode::Plain) {
            out.push(
                "Hay cuentas con TLS desactivado: las credenciales viajarán en claro."
                    .to_string(),
            );
        }
        if self.accounts.iter().any(|a| {
            a.password
                .as_ref()
                .is_some_and(|p| !p.is_empty())
        }) {
            out.push(
                "Hay contraseñas guardadas en el archivo de configuración. Usá 'Guardar sin contraseñas' o password_env."
                    .to_string(),
            );
        }
        if self.zip_mode == ZipMode::None && self.cleanup_raw_after_zip {
            out.push("cleanup_raw_after_zip no tiene efecto cuando zip_mode = None.".to_string());
        }
        if self.hash_verify {
            out.push(
                "hash_verify calcula SHA-256 de cada mensaje: el backup es más lento pero detecta corrupción local."
                    .to_string(),
            );
        }
        out
    }

    pub fn account_by_email(&self, email: &str) -> Option<&AccountConfig> {
        self.accounts
            .iter()
            .find(|a| a.email.eq_ignore_ascii_case(email))
    }
}

// ---------------------------------------------------------------------------
// Secretos
// ---------------------------------------------------------------------------

/// Archivo de credenciales separado de la configuración, con permisos 0600 en Unix.
/// No requiere dependencias nativas (a diferencia de un llavero del sistema) y
/// permite compartir el `config.toml` sin exponer contraseñas.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Secrets {
    #[serde(default)]
    pub passwords: BTreeMap<String, String>,
}

impl Secrets {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|content| toml::from_str(&content).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let content = toml::to_string_pretty(self)?;
        crate::fsutil::write_atomic(path, content.as_bytes())?;
        crate::fsutil::restrict_permissions(path);
        Ok(())
    }

    pub fn set(&mut self, email: &str, password: &str) {
        self.passwords
            .insert(email.to_string(), clean_secret(password));
    }

    pub fn remove(&mut self, email: &str) {
        self.passwords.remove(email);
    }

    pub fn get(&self, email: &str) -> Option<&String> {
        self.passwords.get(email)
    }
}

// ---------------------------------------------------------------------------
// Rutas
// ---------------------------------------------------------------------------

/// Directorio de configuración del usuario (`~/.config/imap-backup` en Linux,
/// `%APPDATA%\imap-backup` en Windows). Si no se puede determinar, se usa el
/// directorio del ejecutable.
pub fn config_dir() -> PathBuf {
    directories_next::ProjectDirs::from("", "", "imap-backup")
        .map(|dirs| dirs.config_dir().to_path_buf())
        .unwrap_or_else(exe_dir)
}

pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Orden de búsqueda del archivo de configuración:
///
/// 1. ruta explícita (`--config`);
/// 2. directorio actual;
/// 3. directorio del ejecutable (modo portable);
/// 4. directorio de configuración del usuario.
///
/// La v1 solo miraba el directorio de trabajo, así que al abrir el ejecutable
/// desde un acceso directo la configuración "desaparecía".
pub fn discover_config_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return if path.exists() { Some(path.to_path_buf()) } else { None };
    }

    let candidates: Vec<PathBuf> = vec![
        PathBuf::from("config.toml"),
        PathBuf::from("config.json"),
        exe_dir().join("config.toml"),
        exe_dir().join("config.json"),
        config_dir().join("config.toml"),
        config_dir().join("config.json"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

/// Ruta del archivo de secretos: junto a la configuración y, si no existe, en el
/// directorio de configuración del usuario.
pub fn secrets_path(config_path: Option<&Path>) -> PathBuf {
    if let Some(cfg) = config_path {
        if let Some(parent) = cfg.parent() {
            let candidate = parent.join("secrets.toml");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    config_dir().join("secrets.toml")
}

/// Nombre de cuenta apto para el sistema de archivos (para logs y reportes).
pub fn safe_account_name(email: &str) -> String {
    let sanitized = sanitize_component(email);
    if sanitized.is_empty() {
        "cuenta".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, content: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        // Cada llamada usa su propio directorio: los tests corren en paralelo y
        // compartir la ruta hacía que se pisaran los archivos entre sí.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "imapb-config-{}-{}-{}",
            name,
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn loads_v1_toml_and_translates_tls_boolean() {
        let toml = r#"
output_dir = "backups"
concurrency_limit = 3
zip_mode = "Consolidated"
cleanup_raw_after_zip = false
retry_attempts = 3
retry_delay_secs = 5
timeout_secs = 45
skip_existing = true

[[accounts]]
email = "contacto@midominio.com"
password = "secreto"
host = "imap.hostinger.com"
port = 993
tls = true
label = "midominio.com"
exclude_folders = ["Spam", "Trash"]

[[accounts]]
email = "legacy@ejemplo.com"
password = "secreto2"
host = "mail.ejemplo.com"
port = 143
tls = false
"#;
        let path = write_temp("config.toml", toml);
        let cfg = AppConfig::load_from_file(&path).unwrap();
        assert_eq!(cfg.zip_mode, ZipMode::Consolidated);
        assert_eq!(cfg.retry_backoff_base_secs, 5); // alias de la v1
        assert_eq!(cfg.accounts[0].tls, TlsMode::Implicit);
        assert_eq!(cfg.accounts[1].tls, TlsMode::Plain);
        assert_eq!(cfg.accounts[1].effective_port(), 143);
        // Valores por defecto nuevos
        assert!(cfg.verify_after_backup);
        assert!(cfg.dedup_on_restore);
        assert_eq!(cfg.export_format, ExportFormat::Eml);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn loads_json_and_accepts_tls_string() {
        let json = r#"{
            "output_dir": "salida",
            "accounts": [
                {
                    "email": "a@b.com",
                    "password": "x",
                    "host": "imap.b.com",
                    "port": 143,
                    "tls": "starttls"
                }
            ]
        }"#;
        let path = write_temp("config.json", json);
        let cfg = AppConfig::load_from_file(&path).unwrap();
        assert_eq!(cfg.accounts[0].tls, TlsMode::StartTls);
        assert_eq!(cfg.concurrency_limit, 3);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rejects_unknown_tls_mode_and_empty_accounts() {
        let bad = r#"{ "accounts": [ { "email": "a@b.com", "password": "x", "host": "h", "tls": "inventado" } ] }"#;
        let path = write_temp("config.json", bad);
        let err = AppConfig::load_from_file(&path).unwrap_err().to_string();
        assert!(err.contains("TOML") || err.contains("JSON") || err.contains("tls") || err.contains("inválida"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        let empty = r#"{ "accounts": [] }"#;
        let path = write_temp("config.json", empty);
        assert!(AppConfig::load_from_file(&path).unwrap_err().to_string().contains("vacía"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn detects_duplicate_accounts_and_missing_secrets() {
        let mut cfg = AppConfig::default();
        cfg.accounts.push(AccountConfig::new("a@b.com", "imap.b.com"));
        cfg.accounts.push(AccountConfig::new("A@B.com", "imap.b.com"));
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("duplicada"));
        assert!(err.contains("no tiene contraseña"));
    }

    #[test]
    fn saving_without_passwords_moves_them_to_secrets() {
        let mut cfg = AppConfig::default();
        let mut acc = AccountConfig::new("a@b.com", "imap.b.com");
        acc.password = Some("super-secreta".to_string());
        cfg.accounts.push(acc.clone());

        let path = std::env::temp_dir().join("imapb-save-test").join("config.toml");
        let serialized = cfg.serialize_for(&path, false).unwrap();
        assert!(!serialized.contains("super-secreta"));

        let serialized_with = cfg.serialize_for(&path, true).unwrap();
        assert!(serialized_with.contains("super-secreta"));

        let mut secrets = Secrets::default();
        secrets.set("a@b.com", "super-secreta");
        assert_eq!(
            acc.resolve_password(&secrets, false).unwrap(),
            "super-secreta"
        );
    }

    #[test]
    fn password_env_is_used_when_field_is_absent() {
        let mut acc = AccountConfig::new("env@b.com", "imap.b.com");
        acc.password_env = Some("IMAPB_TEST_PASSWORD".to_string());
        std::env::set_var("IMAPB_TEST_PASSWORD", "desde-entorno");
        let secrets = Secrets::default();
        assert_eq!(acc.resolve_password(&secrets, false).unwrap(), "desde-entorno");
        std::env::remove_var("IMAPB_TEST_PASSWORD");
    }

    #[test]
    fn secrets_roundtrip_and_permissions() {
        let dir = std::env::temp_dir().join(format!("imapb-secrets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secrets.toml");
        let mut secrets = Secrets::default();
        secrets.set("a@b.com", "pwd");
        secrets.save(&path).unwrap();
        let loaded = Secrets::load(&path);
        assert_eq!(loaded.get("a@b.com").unwrap(), "pwd");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_secret_strips_crlf() {
        assert_eq!(clean_secret("pwd\r\nINJECT"), "pwdINJECT");
    }

    #[test]
    fn warnings_flag_insecure_and_plaintext_password() {
        let mut cfg = AppConfig::default();
        let mut acc = AccountConfig::new("a@b.com", "h");
        acc.tls = TlsMode::Plain;
        acc.password = Some("x".into());
        cfg.accounts.push(acc);
        let warnings = cfg.warnings();
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn discovery_prefers_explicit_path_then_existing_candidate() {
        let dir = std::env::temp_dir().join(format!("imapb-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let explicit = dir.join("mi-config.toml");
        std::fs::write(&explicit, "accounts = []").unwrap();
        assert_eq!(discover_config_path(Some(&explicit)), Some(explicit.clone()));
        assert_eq!(discover_config_path(Some(&dir.join("nope.toml"))), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
