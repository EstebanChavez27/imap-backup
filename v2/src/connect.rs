// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Capa de conexión IMAP compartida por backup, restauración y migración.
//!
//! Puntos que la v1 resolvía mal o directamente ignoraba:
//!
//! * **Modo de cifrado real**: `Tls` / `STARTTLS` / `Plaintext` según la
//!   configuración, en vez de forzar siempre TLS directo e ignorar el campo `tls`.
//! * **Timeout de lectura**: se aplica sobre la conexión ya establecida (la v1 lo
//!   ponía sobre el `TcpStream` antes del handshake y lo perdía con STARTTLS).
//! * **Sesión reutilizada**: una sola conexión por trabajador en lugar de un
//!   `LOGIN` por cada lote de 25 mensajes. Menos handshakes TLS, menos riesgo de que
//!   el hosting corte la cuenta por rate-limit.
//! * **OAuth2 (XOAUTH2)**: necesario para Microsoft 365 y Google Workspace.
//! * **Backoff exponencial con jitter** en lugar de espera lineal fija.

use crate::config::{AccountConfig, Secrets, TlsMode};
use crate::events::Ctx;
use crate::folders::SpecialUse;
use anyhow::{anyhow, bail, Context, Result};
use imap::extensions::idle::SetReadTimeout;
use imap::{Connection, ConnectionMode, Session};
use std::time::Duration;

pub type ImapSession = Session<Connection>;

#[derive(Debug, Clone)]
pub struct ConnOptions {
    pub host: String,
    pub port: u16,
    pub tls: TlsMode,
    pub email: String,
    pub password: Option<String>,
    pub oauth_token: Option<String>,
    pub timeout: Duration,
    pub insecure_skip_tls_verify: bool,
}

impl ConnOptions {
    pub fn from_account(account: &AccountConfig, secrets: &Secrets) -> Result<Self> {
        Ok(Self {
            host: crate::config::clean_secret(account.host.trim()),
            port: account.effective_port(),
            tls: account.tls,
            email: crate::config::clean_secret(account.email.trim()),
            password: Some(account.resolve_password(secrets, true)?),
            oauth_token: account.resolve_oauth_token(),
            timeout: Duration::from_secs(45),
            insecure_skip_tls_verify: account.insecure_skip_tls_verify,
        })
    }

    pub fn with_timeout(mut self, timeout_secs: u64) -> Self {
        self.timeout = Duration::from_secs(timeout_secs.max(1));
        self
    }

    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(crate::config::clean_secret(&password.into()));
        self
    }

    pub fn describe(&self) -> String {
        format!(
            "{}@{}:{} ({})",
            self.email,
            self.host,
            self.port,
            match self.tls {
                TlsMode::Implicit => "TLS",
                TlsMode::StartTls => "STARTTLS",
                TlsMode::Plain => "sin cifrado",
            }
        )
    }

    fn connection_mode(&self) -> ConnectionMode {
        match self.tls {
            TlsMode::Implicit => ConnectionMode::Tls,
            TlsMode::StartTls => ConnectionMode::StartTls,
            TlsMode::Plain => ConnectionMode::Plaintext,
        }
    }
}

/// Autenticador SASL XOAUTH2 (RFC 7628), requerido por los proveedores que ya no
/// aceptan usuario/contraseña.
struct Xoauth2 {
    user: String,
    token: String,
}

impl imap::Authenticator for Xoauth2 {
    type Response = String;

    fn process(&self, _challenge: &[u8]) -> Self::Response {
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token)
    }
}

/// Abre la conexión y se autentica (LOGIN o XOAUTH2).
pub fn connect_and_login(opts: &ConnOptions) -> Result<ImapSession> {
    if opts.host.trim().is_empty() {
        bail!("Host IMAP vacío");
    }

    let client = imap::ClientBuilder::new(opts.host.as_str(), opts.port)
        .mode(opts.connection_mode())
        .danger_skip_tls_verify(opts.insecure_skip_tls_verify)
        .connect()
        .with_context(|| {
            format!(
                "No se pudo conectar con {}:{} en modo {}",
                opts.host,
                opts.port,
                match opts.tls {
                    TlsMode::Implicit => "TLS",
                    TlsMode::StartTls => "STARTTLS",
                    TlsMode::Plain => "texto plano",
                }
            )
        })?;

    // El constructor de la crate no expone el timeout, que igual hay que fijar:
    // sin él, un servidor que deja el socket abierto bloquea el hilo para siempre.
    let mut connection = client
        .into_inner()
        .map_err(|err| anyhow!("Error obteniendo la conexión: {}", err))?;
    connection
        .set_read_timeout(Some(opts.timeout))
        .with_context(|| "No se pudo configurar el timeout de lectura")?;
    let client = imap::Client::new(connection);

    if let Some(token) = opts.oauth_token.as_ref().filter(|t| !t.trim().is_empty()) {
        let authenticator = Xoauth2 {
            user: opts.email.clone(),
            token: token.clone(),
        };
        let session = client
            .authenticate("XOAUTH2", &authenticator)
            .map_err(|(err, _)| anyhow!("Fallo de autenticación XOAUTH2: {}", err))?;
        return Ok(session);
    }

    let password = opts
        .password
        .as_ref()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| anyhow!("No hay contraseña ni token OAuth2 para {}", opts.email))?;

    client
        .login(&opts.email, password)
        .map_err(|(err, _)| anyhow!("Fallo de autenticación IMAP: {}", err))
}

/// Mantiene una sesión abierta y la reconecta solo cuando hace falta.
pub struct SessionHolder {
    opts: ConnOptions,
    label: String,
    session: Option<ImapSession>,
    connects: u64,
}

impl SessionHolder {
    pub fn new(opts: ConnOptions, label: impl Into<String>) -> Self {
        Self {
            opts,
            label: label.into(),
            session: None,
            connects: 0,
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn describe(&self) -> String {
        self.opts.describe()
    }

    pub fn connects(&self) -> u64 {
        self.connects
    }

    pub fn is_connected(&self) -> bool {
        self.session.is_some()
    }

    /// Devuelve una sesión lista para usar, conectando si es necesario.
    pub fn session(&mut self) -> Result<&mut ImapSession> {
        if self.session.is_none() {
            let session = connect_and_login(&self.opts)?;
            self.connects += 1;
            self.session = Some(session);
        }
        Ok(self.session.as_mut().expect("sesión recién creada"))
    }

    /// Marca la sesión como caída: la próxima llamada reconectará.
    pub fn invalidate(&mut self) {
        if let Some(mut session) = self.session.take() {
            let _ = session.logout();
        }
    }

    pub fn disconnect(&mut self) {
        if let Some(mut session) = self.session.take() {
            let _ = session.logout();
        }
    }
}

/// Espera antes del intento `attempt` (1-based), exponencial y acotada.
///
/// `base * 2^(attempt-1)` segundos, limitado por `max`, más un jitter determinista
/// derivado de `jitter_seed` para que varias cuentas no reintenten al mismo tiempo
/// y vuelvan a chocar contra el rate-limit del hosting.
pub fn backoff_millis(base_secs: u64, max_secs: u64, attempt: u32, jitter_seed: u64) -> u64 {
    let base_secs = base_secs.max(1);
    let max_secs = max_secs.max(base_secs);
    let exponent = attempt.saturating_sub(1).min(16);
    let raw = base_secs.saturating_mul(1u64 << exponent.min(20));
    let capped = raw.min(max_secs);
    let jitter_ms = jitter_seed % 1000;
    (capped.saturating_mul(1000)).saturating_add(jitter_ms)
}

fn jitter_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 + d.as_secs())
        .unwrap_or(0)
}

/// Ejecuta una operación con reintentos: invalida la sesión ante cualquier error,
/// espera con backoff y vuelve a intentar con una conexión nueva.
pub fn with_retry<T>(
    ctx: &Ctx,
    holder: &mut SessionHolder,
    op_name: &str,
    attempts: u32,
    backoff_base_secs: u64,
    backoff_max_secs: u64,
    mut op: impl FnMut(&mut ImapSession) -> Result<T>,
) -> Result<T> {
    let attempts = attempts.max(1);
    let mut last_error: Option<anyhow::Error> = None;

    for attempt in 1..=attempts {
        ctx.check_cancel()?;

        let result = match holder.session() {
            Ok(session) => op(session),
            Err(err) => Err(err),
        };

        match result {
            Ok(value) => return Ok(value),
            Err(err) => {
                if crate::cancel::is_cancelled(&err) {
                    return Err(err);
                }
                let wait_ms = backoff_millis(backoff_base_secs, backoff_max_secs, attempt, jitter_seed());
                if attempt < attempts {
                    ctx.warn(
                        holder.label(),
                        format!(
                            "{} falló (intento {}/{}): {}. Reintentando en {:.1}s",
                            op_name,
                            attempt,
                            attempts,
                            err,
                            wait_ms as f64 / 1000.0
                        ),
                    );
                } else {
                    ctx.error(
                        holder.label(),
                        format!("{} falló definitivamente tras {} intento(s): {}", op_name, attempts, err),
                    );
                }
                holder.invalidate();
                last_error = Some(err);

                if attempt < attempts {
                    sleep_interruptible(ctx, wait_ms)?;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("{} falló sin error registrado", op_name)))
}

/// Espera cancelable: el usuario puede detener un backup que está en backoff.
fn sleep_interruptible(ctx: &Ctx, total_ms: u64) -> Result<()> {
    let step = Duration::from_millis(200);
    let mut waited = 0u64;
    while waited < total_ms {
        ctx.check_cancel()?;
        let chunk = step.min(Duration::from_millis(total_ms - waited));
        std::thread::sleep(chunk);
        waited += chunk.as_millis() as u64;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFolder {
    pub name: String,
    pub delimiter: Option<char>,
    pub special_use: Option<SpecialUse>,
    pub selectable: bool,
}

/// Lista los buzones del servidor con delimitador, carpetas especiales y si son
/// seleccionables (`\Noselect`, p. ej. `[Gmail]`).
pub fn list_folders(session: &mut ImapSession) -> Result<Vec<RemoteFolder>> {
    let names = session
        .list(None, Some("*"))
        .context("Error ejecutando LIST")?;

    let mut folders: Vec<RemoteFolder> = Vec::new();
    for name in names.iter() {
        let attributes = name.attributes();
        let special_use = SpecialUse::from_attributes(attributes)
            .or_else(|| SpecialUse::from_name(name.name()));
        folders.push(RemoteFolder {
            name: name.name().to_string(),
            delimiter: name.delimiter().and_then(|d| d.chars().next()),
            special_use,
            selectable: crate::folders::is_selectable(attributes),
        });
    }
    folders.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(folders)
}

#[derive(Debug, Clone)]
pub struct MailboxInfo {
    pub exists: u64,
    pub uid_validity: u32,
    pub uid_next: u32,
    pub is_read_only: bool,
}

/// Selecciona (o examina en modo lectura) una carpeta y devuelve sus metadatos.
pub fn select_mailbox(
    session: &mut ImapSession,
    folder: &str,
    read_only: bool,
) -> Result<MailboxInfo> {
    let mailbox = if read_only {
        session
            .examine(folder)
            .with_context(|| format!("No se pudo abrir (solo lectura) la carpeta '{}'", folder))?
    } else {
        session
            .select(folder)
            .with_context(|| format!("No se pudo abrir la carpeta '{}'", folder))?
    };

    Ok(MailboxInfo {
        exists: mailbox.exists as u64,
        uid_validity: mailbox.uid_validity.unwrap_or(0),
        uid_next: mailbox.uid_next.unwrap_or(0),
        is_read_only: mailbox.is_read_only,
    })
}

/// Pide metadatos (sin cuerpos) de un rango de UIDs.
pub fn fetch_metadata(
    session: &mut ImapSession,
    uid_range: &str,
) -> Result<Vec<crate::manifest::ServerMessage>> {
    let fetches = session
        .uid_fetch(uid_range, "(UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)")
        .with_context(|| format!("Error en UID FETCH de metadatos ({})", uid_range))?;

    let mut out = Vec::with_capacity(fetches.len());
    for fetch in fetches.iter() {
        let Some(uid) = fetch.uid else { continue };
        out.push(crate::manifest::ServerMessage {
            uid,
            size: fetch.size.unwrap_or(0) as u64,
            message_id: fetch
                .envelope()
                .and_then(|env| env.message_id.as_ref())
                .map(|id| String::from_utf8_lossy(id).to_string()),
            internal_date: fetch.internal_date().map(|d| d.to_rfc3339()),
            flags: fetch.flags().iter().map(|f| f.to_string()).collect(),
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Default)]
pub struct FetchedMessage {
    pub uid: u32,
    pub message_id: Option<String>,
    pub internal_date: Option<String>,
    pub flags: Vec<String>,
    pub size: u64,
    pub body: Option<Vec<u8>>,
}

/// Descarga los cuerpos de un conjunto de UIDs (usando `BODY.PEEK[]` para no marcar
/// los mensajes como leídos en el servidor).
pub fn fetch_bodies(
    session: &mut ImapSession,
    uid_set: &str,
) -> Result<Vec<FetchedMessage>> {
    let fetches = session
        .uid_fetch(uid_set, "(BODY.PEEK[] UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)")
        .with_context(|| format!("Error en UID FETCH de cuerpos ({})", uid_set))?;

    let mut out = Vec::with_capacity(fetches.len());
    for fetch in fetches.iter() {
        let Some(uid) = fetch.uid else { continue };
        let body = fetch.body().or_else(|| fetch.text()).map(|b| b.to_vec());
        out.push(FetchedMessage {
            uid,
            message_id: fetch
                .envelope()
                .and_then(|env| env.message_id.as_ref())
                .map(|id| String::from_utf8_lossy(id).to_string()),
            internal_date: fetch.internal_date().map(|d| d.to_rfc3339()),
            flags: fetch.flags().iter().map(|f| f.to_string()).collect(),
            size: fetch.size.unwrap_or(0) as u64,
            body,
        });
    }
    Ok(out)
}

/// Escapa un nombre de buzón para `APPEND`.
///
/// La crate cita el nombre con `"` pero **no** escapa su contenido (a diferencia de
/// `SELECT`/`EXAMINE`/`CREATE`, que sí lo hacen vía `validate_str`). Sin esto, una
/// carpeta con comillas rompería el comando.
pub fn escape_mailbox_for_append(name: &str) -> String {
    name.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Sube un mensaje con `APPEND` preservando flags y fecha interna.
pub fn append_message(
    session: &mut ImapSession,
    folder: &str,
    raw: &[u8],
    flags: &[String],
    internal_date: Option<&str>,
) -> Result<()> {
    let mailbox = escape_mailbox_for_append(folder);
    let date = internal_date
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.fixed_offset());

    let mut command = session.append(&mailbox, raw);
    for flag in flags {
        let cleaned = flag.trim();
        if cleaned.is_empty() || cleaned.eq_ignore_ascii_case("\\Recent") {
            continue;
        }
        command.flag(imap::types::Flag::from(cleaned.to_string()));
    }
    if let Some(date) = date {
        command.internal_date(date);
    }
    command.finish().map_err(|err| anyhow!("APPEND en '{}' falló: {}", folder, err))?;
    Ok(())
}

/// Conjunto normalizado de `Message-ID` ya presentes en una carpeta del destino.
/// Se piden solo las cabeceras (barato) para poder restaurar de forma idempotente.
pub fn existing_message_ids(session: &mut ImapSession, folder: &str) -> Result<std::collections::HashSet<String>> {
    let mut ids = std::collections::HashSet::new();
    let mailbox = select_mailbox(session, folder, true)?;
    if mailbox.exists == 0 {
        return Ok(ids);
    }
    let fetches = session
        .uid_fetch("1:*", "(UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])")
        .with_context(|| format!("Error leyendo Message-ID de '{}'", folder))?;
    for fetch in fetches.iter() {
        if let Some(body) = fetch.body().or_else(|| fetch.text()) {
            if let Some(id) = crate::formats::header_value(body, "message-id") {
                ids.insert(normalize_message_id(&id));
            }
        }
    }
    Ok(ids)
}

/// Normaliza un `Message-ID` para comparar: sin espacios, sin `<>` y en minúsculas.
pub fn normalize_message_id(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_mailbox_names_are_escaped_for_quotes_and_backslashes() {
        assert_eq!(escape_mailbox_for_append("INBOX"), "INBOX");
        assert_eq!(
            escape_mailbox_for_append("Carpeta \"rara\""),
            "Carpeta \\\"rara\\\""
        );
        assert_eq!(escape_mailbox_for_append("a\\b"), "a\\\\b");
    }

    #[test]
    fn message_ids_are_normalized_for_comparison() {
        assert_eq!(normalize_message_id(" <ABC@Host.COM> "), "abc@host.com");
        assert_eq!(normalize_message_id("abc@host.com"), "abc@host.com");
        assert_eq!(normalize_message_id("<"), "");
    }

    #[test]
    fn backoff_grows_exponentially_and_respects_cap() {
        assert_eq!(backoff_millis(1, 60, 1, 0), 1_000);
        assert_eq!(backoff_millis(1, 60, 2, 0), 2_000);
        assert_eq!(backoff_millis(1, 60, 3, 0), 4_000);
        assert_eq!(backoff_millis(1, 60, 20, 0), 60_000);
        assert_eq!(backoff_millis(30, 60, 3, 0), 60_000);
        // El jitter siempre queda por debajo de un segundo.
        assert_eq!(backoff_millis(1, 60, 1, 500), 1_500);
    }

    #[test]
    fn backoff_handles_zero_attempt_and_zero_base() {
        assert_eq!(backoff_millis(0, 0, 0, 0), 1_000);
        assert!(backoff_millis(1, 60, 1, 0) <= backoff_millis(1, 60, 2, 0));
    }

    #[test]
    fn connection_mode_follows_tls_setting() {
        let mut opts = ConnOptions {
            host: "h".into(),
            port: 993,
            tls: TlsMode::Implicit,
            email: "a@b".into(),
            password: Some("p".into()),
            oauth_token: None,
            timeout: Duration::from_secs(5),
            insecure_skip_tls_verify: false,
        };
        assert!(matches!(opts.connection_mode(), ConnectionMode::Tls));
        opts.tls = TlsMode::StartTls;
        assert!(matches!(opts.connection_mode(), ConnectionMode::StartTls));
        opts.tls = TlsMode::Plain;
        assert!(matches!(opts.connection_mode(), ConnectionMode::Plaintext));
    }

    #[test]
    fn options_from_account_prefer_env_over_secrets() {
        let mut account = AccountConfig::new("acc@b.com", " imap.b.com ");
        account.password = Some("directa\r\n".into());
        let secrets = Secrets::default();
        let opts = ConnOptions::from_account(&account, &secrets).unwrap();
        assert_eq!(opts.host, "imap.b.com");
        assert_eq!(opts.password.as_deref(), Some("directa"));
        assert_eq!(opts.tls, TlsMode::Implicit);
    }

    #[test]
    fn describe_reports_requested_mode() {
        let opts = ConnOptions {
            host: "imap.b.com".into(),
            port: 143,
            tls: TlsMode::StartTls,
            email: "a@b.com".into(),
            password: None,
            oauth_token: None,
            timeout: Duration::from_secs(5),
            insecure_skip_tls_verify: false,
        };
        assert!(opts.describe().contains("STARTTLS"));
    }

    #[test]
    fn there_is_no_imap_without_host() {
        let opts = ConnOptions {
            host: "".into(),
            port: 993,
            tls: TlsMode::Implicit,
            email: "a@b.com".into(),
            password: Some("p".into()),
            oauth_token: None,
            timeout: Duration::from_secs(1),
            insecure_skip_tls_verify: false,
        };
        assert!(connect_and_login(&opts).is_err());
    }
}
