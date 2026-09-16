// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Modo headless (línea de comandos).
//!
//! Es lo que convierte el respaldo en algo que ocurre solo: `imap-backup backup`
//! desde el Programador de tareas de Windows o desde cron, con códigos de salida
//! utilizables por scripts y log a archivo.

use crate::backup::{self, BackupOptions};
use crate::config::{
    discover_config_path, secrets_path, AppConfig, ExportFormat, Secrets, TlsMode, ZipCompression,
    ZipMode,
};
use crate::connect::ConnOptions;
use crate::events::{Ctx, Logger};
use crate::migrate::{self, MigrateOptions};
use crate::report::RunReport;
use crate::restore::{self, RestoreOptions};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub const EXIT_OK: i32 = 0;
pub const EXIT_ERRORS: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_FATAL: i32 = 3;
pub const EXIT_CANCELLED: i32 = 130;

pub enum Outcome {
    Exit(i32),
    LaunchGui,
}

/// Error de uso o de configuración: lo que hay que corregir es la invocación (o el
/// archivo de configuración), no un fallo del programa. Se distingue para que los
/// scripts puedan separar «invocación mal armada» (código 2) de «algo se rompió» (3).
#[derive(Debug)]
pub struct UsageError(String);

impl UsageError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// Convierte en error de uso un `Option`/`Result` que debería haber traído un valor.
trait UsageExt<T> {
    fn usage(self, message: impl Into<String>) -> Result<T>;
}

impl<T> UsageExt<T> for Option<T> {
    fn usage(self, message: impl Into<String>) -> Result<T> {
        self.ok_or_else(|| anyhow::Error::new(UsageError::new(message)))
    }
}

impl<T, E: std::fmt::Display> UsageExt<T> for std::result::Result<T, E> {
    fn usage(self, message: impl Into<String>) -> Result<T> {
        match self {
            Ok(value) => Ok(value),
            Err(err) => Err(anyhow::Error::new(UsageError::new(message)).context(err.to_string())),
        }
    }
}

/// Igual que `bail!` pero marcando el error como problema de uso (código 2).
macro_rules! usage_bail {
    ($($arg:tt)*) => {
        return Err(anyhow::Error::new(UsageError::new(format!($($arg)*))))
    };
}

pub const HELP: &str = r#"imap-backup 2.0 — respaldo, verificación y migración de correo IMAP

USO
  imap-backup <comando> [opciones]

COMANDOS
  backup              Respalda las cuentas de la configuración (incremental real).
  restore             Restaura un backup (ZIP, carpeta .eml, mbox o Maildir) a un buzón.
  migrate             Migra mensajes directamente de un servidor IMAP a otro.
  test-connection     Verifica credenciales y lista las carpetas de cada cuenta.
  verify              Comprueba la integridad del backup local contra su manifiesto
                      (no necesita configuración: descubre las cuentas en --output).
  extract             Extrae un ZIP de respaldo a una carpeta.
  gui                 Abre la interfaz gráfica.
  help                Muestra esta ayuda.

OPCIONES GENERALES
  -c, --config RUTA        Archivo de configuración (TOML o JSON).
  -o, --output DIR         Directorio destino del backup.
      --account EMAIL      Limita la operación a esa cuenta (repetible).
      --concurrency N      Cuentas en paralelo durante el backup.
      --restore-concurrency N  Carpetas/mensajes en paralelo al restaurar o migrar.
      --zip MODO           per-account | consolidated | none
      --format FORMATO     eml | mbox | maildir
      --compression MODO   deflate | store
      --cleanup-after-zip  Borra los archivos sueltos tras comprimir.
      --force-full         Ignora el manifiesto y vuelve a bajar todo.
      --dry-run            Analiza y reporta sin escribir ni subir nada.
      --no-verify          Omite la verificación final contra el servidor.
      --log-file RUTA      Escribe el log a un archivo.
      --report RUTA        Escribe el reporte (.json y .csv) en esa ruta base.
      --json               Imprime el reporte JSON en la salida estándar.
      --quiet              No imprime los logs en consola.
      --password-env VAR   Variable de entorno con la contraseña de las cuentas.

OPCIONES DE RESTORE
      --source RUTA        Archivo .zip/.mbox o carpeta con el backup (obligatorio).
      --dest-host HOST     Servidor IMAP de destino.
      --dest-port N        Puerto del destino (por defecto 993).
      --dest-email EMAIL   Usuario del destino.
      --dest-tls MODO      implicit | starttls | plain (por defecto implicit).
      --dest-account EMAIL Usa esa cuenta de la configuración como destino.
      --dest-password-env VAR  Variable de entorno con la contraseña del destino.
      --prefix NOMBRE      Restaura dentro de un prefijo de carpeta (p. ej. "Migrado").
      --no-dedup           No deduplica por Message-ID (puede duplicar correos).

OPCIONES DE MIGRATE
      --source-account EMAIL  Cuenta origen (de la configuración).
      --dest-account EMAIL    Cuenta destino (de la configuración).
      --keep-local DIR        Además de migrar, deja una copia local en DIR.

CÓDIGOS DE SALIDA
  0 éxito · 1 finalizó con errores · 2 error de uso/configuración
  3 error inesperado · 130 cancelado por el usuario
"#;

/// Analizador de argumentos con soporte de banderas repetibles.
#[derive(Debug, Default, Clone)]
pub struct Args {
    pub command: String,
    pub values: HashMap<String, Vec<String>>,
    pub flags: Vec<String>,
}

/// ¿El token parece el nombre del ejecutable (`imap-backup`, `imap-backup.exe`,
/// una ruta a `target/debug/imap-backup`, …)? Sirve para descartar `argv[0]`
/// sin depender de que quien llama ya lo haya quitado.
fn is_program_name(token: &str) -> bool {
    let file = token.rsplit(['/', '\\']).next().unwrap_or(token);
    let stem = file.strip_suffix(".exe").unwrap_or(file);
    stem == "imap-backup" || stem.starts_with("imap-backup-")
}

impl Args {
    pub fn parse(argv: &[String]) -> Result<Self> {
        // `argv` llega tal cual desde el sistema operativo: el primer elemento es
        // el nombre del ejecutable y se descarta cuando lo reconocemos.
        let argv: &[String] = match argv.split_first() {
            Some((first, rest)) if is_program_name(first) => rest,
            _ => argv,
        };

        let mut args = Args {
            command: String::new(),
            values: HashMap::new(),
            flags: Vec::new(),
        };

        let known_commands = [
            "backup",
            "restore",
            "migrate",
            "test-connection",
            "verify",
            "extract",
            "gui",
            "help",
            "version",
        ];
        let takes_value = [
            "config",
            "c",
            "output",
            "o",
            "account",
            "concurrency",
            "restore-concurrency",
            "zip",
            "format",
            "compression",
            "log-file",
            "report",
            "password-env",
            "source",
            "dest-host",
            "dest-port",
            "dest-email",
            "dest-tls",
            "dest-account",
            "dest-password-env",
            "prefix",
            "source-account",
            "keep-local",
        ];

        let mut index = 0usize;
        while index < argv.len() {
            let token = &argv[index];
            if let Some(rest) = token.strip_prefix("--") {
                let (name, inline) = match rest.split_once('=') {
                    Some((name, value)) => (name.to_string(), Some(value.to_string())),
                    None => (rest.to_string(), None),
                };
                if takes_value.contains(&name.as_str()) {
                    let value = match inline {
                        Some(value) => value,
                        None => {
                            index += 1;
                            argv.get(index)
                                .cloned()
                                .with_context(|| format!("Falta el valor de --{}", name))?
                        }
                    };
                    args.values.entry(name).or_default().push(value);
                } else {
                    args.flags.push(name);
                }
            } else if let Some(rest) = token.strip_prefix('-') {
                let name = rest.to_string();
                if takes_value.contains(&name.as_str()) {
                    index += 1;
                    let value = argv
                        .get(index)
                        .cloned()
                        .with_context(|| format!("Falta el valor de -{}", name))?;
                    let canonical = match name.as_str() {
                        "c" => "config",
                        "o" => "output",
                        other => other,
                    };
                    args.values.entry(canonical.to_string()).or_default().push(value);
                } else {
                    args.flags.push(name);
                }
            } else if args.command.is_empty() && known_commands.contains(&token.as_str()) {
                args.command = token.to_string();
            } else if args.command.is_empty() {
                bail!("Comando desconocido: '{}'. Ejecutá 'imap-backup help'.", token);
            } else {
                bail!("Argumento inesperado: '{}'", token);
            }
            index += 1;
        }

        if args.command.is_empty() {
            args.command = "gui".to_string();
        }
        Ok(args)
    }

    pub fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|flag| flag == name)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .and_then(|values| values.last())
            .map(|value| value.as_str())
    }

    pub fn values_of(&self, name: &str) -> Vec<String> {
        self.values.get(name).cloned().unwrap_or_default()
    }

    fn number(&self, name: &str) -> Result<Option<u64>> {
        match self.value(name) {
            Some(raw) => match raw.trim().parse::<u64>() {
                Ok(value) => Ok(Some(value)),
                Err(_) => usage_bail!("--{} debe ser un número (recibido '{}')", name, raw),
            },
            None => Ok(None),
        }
    }
}


/// Punto de entrada sin GUI: devuelve el código de salida o pide lanzar la interfaz.
pub fn dispatch(argv: Vec<String>) -> Outcome {
    if argv.len() < 2 {
        return Outcome::LaunchGui;
    }
    let args = match Args::parse(&argv) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("Error: {}", err);
            eprintln!("\n{}", HELP);
            return Outcome::Exit(EXIT_USAGE);
        }
    };

    match args.command.as_str() {
        "help" | "--help" => {
            println!("{}", HELP);
            Outcome::Exit(EXIT_OK)
        }
        "version" => {
            println!("imap-backup {}", env!("CARGO_PKG_VERSION"));
            Outcome::Exit(EXIT_OK)
        }
        "gui" => Outcome::LaunchGui,
        _ => Outcome::Exit(run_command(args)),
    }
}

fn run_command(args: Args) -> i32 {
    match run_command_inner(&args) {
        Ok(report) => {
            if args.has("json") {
                if let Some(report) = &report {
                    match report.to_json() {
                        Ok(json) => println!("{}", json),
                        Err(err) => eprintln!("No se pudo serializar el reporte: {}", err),
                    }
                }
            } else if let Some(report) = &report {
                println!("\n{}", report.summary_line());
            }

            match &report {
                Some(report) if report.cancelled => EXIT_CANCELLED,
                Some(report) if report.has_errors() => EXIT_ERRORS,
                _ => EXIT_OK,
            }
        }
        Err(err) => {
            if crate::cancel::is_cancelled(&err) {
                eprintln!("Cancelado: {}", err);
                EXIT_CANCELLED
            } else if let Some(usage) = err.downcast_ref::<UsageError>() {
                eprintln!("Error: {}", usage);
                eprintln!("\n{}", HELP);
                EXIT_USAGE
            } else {
                eprintln!("Error: {:#}", err);
                EXIT_FATAL
            }
        }
    }
}

fn run_command_inner(args: &Args) -> Result<Option<RunReport>> {
    if args.has("version") {
        println!("imap-backup {}", env!("CARGO_PKG_VERSION"));
        return Ok(None);
    }

    // `verify` no exige configuración: si no hay cuentas, revisa las que descubra
    // dentro del directorio de backup (que es cuando más falta hace).
    let needs_config = matches!(
        args.command.as_str(),
        "backup" | "test-connection" | "migrate" | "restore"
    );
    let explicit = args.value("config").map(PathBuf::from);
    let config_path = discover_config_path(explicit.as_deref());

    if config_path.is_none() && needs_config && args.value("dest-host").is_none() {
        usage_bail!(
            "No se encontró ningún archivo de configuración.\n\
             Buscado en: directorio actual, junto al ejecutable y en {}.\n\
             Creá un config.toml (mirá config.example.toml) o pasá --config RUTA.",
            crate::config::config_dir().display()
        );
    }

    // En `restore` un archivo de configuración presente pero inválido (por ejemplo,
    // sin cuentas) no debe impedir restaurar hacia un destino indicado por flags.
    let lenient = args.command == "restore" && args.value("dest-host").is_some();
    let mut config = match &config_path {
        Some(path) => match AppConfig::load_from_file(path) {
            Ok(config) => config,
            Err(err) if lenient => {
                eprintln!("Aviso: la configuración no es válida ({}); se usan valores por defecto para restaurar.", err);
                AppConfig::default()
            }
            Err(err) => return Err(err),
        },
        None => AppConfig::default(),
    };

    let secrets = Secrets::load(&secrets_path(config_path.as_deref()));
    apply_overrides(&mut config, args)?;

    let logger = Arc::new(Logger::with_capacity(8000));
    logger.set_console(!args.has("quiet"));
    if let Some(path) = args.value("log-file").map(PathBuf::from) {
        logger
            .set_log_file(&path, crate::events::LogLevel::Info)
            .with_context(|| format!("No se pudo abrir el log {}", path.display()))?;
    }
    if let Some(path) = &config_path {
        logger.log(
            crate::events::LogLevel::Info,
            "cli",
            format!("Configuración: {}", path.display()),
        );
    }
    for warning in config.warnings() {
        logger.log(crate::events::LogLevel::Warning, "config", warning);
    }

    let ctx = Ctx::headless(Arc::clone(&logger), crate::cancel::CancelToken::new());

    let report = match args.command.as_str() {
        "backup" => {
            let mut opts = BackupOptions::from_config(&config);
            opts.dry_run = args.has("dry-run");
            opts.force_full = args.has("force-full");
            opts.verify = !args.has("no-verify");
            let report = backup::run(&config, &secrets, &ctx, &opts)?;
            run_post_hooks(&config, &report, &ctx);
            Some(report)
        }
        "verify" => Some(crate::verify::verify_all(
            &config,
            &config.output_dir,
            &ctx,
        )?),
        "test-connection" => {
            test_connections(&config, &secrets, &ctx)?;
            None
        }
        "restore" => {
            let source = args
                .value("source")
                .map(PathBuf::from)
                .usage("Falta --source con el ZIP, mbox o carpeta a restaurar")?;
            let dest = build_dest_conn(args, &config, &secrets)?;
            let index = restore::build_index(&ctx, &source)?;
            let mut opts = RestoreOptions::from_config(&config, source, dest);
            opts.concurrency = args
                .number("restore-concurrency")?
                .map(|value| value as usize)
                .unwrap_or(opts.concurrency);
            opts.dry_run = args.has("dry-run");
            opts.skip_existing = !args.has("no-dedup");
            opts.folder_prefix = args.value("prefix").map(|value| value.to_string());
            opts.verify = !args.has("no-verify");
            let report = restore::run(index, &opts, &ctx)?;
            run_post_hooks(&config, &report, &ctx);
            Some(report)
        }
        "migrate" => {
            let source_email = args
                .value("source-account")
                .usage("Falta --source-account con el correo de la cuenta origen")?;
            let source_account = config
                .account_by_email(source_email)
                .cloned()
                .usage(format!("La cuenta '{}' no está en la configuración", source_email))?;
            let source_conn = ConnOptions::from_account(&source_account, &secrets)?
                .with_timeout(config.timeout_secs);
            let dest_conn = build_dest_conn(args, &config, &secrets)?;
            let mut opts = MigrateOptions::from_config(&config, source_account, source_conn, dest_conn);
            opts.concurrency = args
                .number("restore-concurrency")?
                .map(|value| value as usize)
                .unwrap_or(opts.concurrency);
            opts.dry_run = args.has("dry-run");
            opts.skip_existing = !args.has("no-dedup");
            opts.folder_prefix = args.value("prefix").map(|value| value.to_string());
            opts.keep_local = args.value("keep-local").map(PathBuf::from);
            opts.verify = !args.has("no-verify");
            let report = migrate::run(&opts, &ctx)?;
            run_post_hooks(&config, &report, &ctx);
            Some(report)
        }
        "extract" => {
            let source = args
                .value("source")
                .map(PathBuf::from)
                .usage("Falta --source con el ZIP a extraer")?;
            let dest = args
                .value("output")
                .map(PathBuf::from)
                .unwrap_or_else(|| source.with_extension(""));
            let extracted = crate::archive::extract_zip(&source, &dest)?;
            println!("Extraídos {} archivo(s) en {}", extracted, dest.display());
            None
        }
        other => bail!("Comando no implementado: {}", other),
    };

    if let (Some(base), Some(report)) = (args.value("report"), report.as_ref()) {
        let paths = report.write_reports(&PathBuf::from(base))?;
        for path in paths {
            println!("Reporte escrito en {}", path.display());
        }
    }

    Ok(report)
}

fn apply_overrides(config: &mut AppConfig, args: &Args) -> Result<()> {
    if let Some(output) = args.value("output") {
        config.output_dir = PathBuf::from(output);
    }
    if let Some(value) = args.number("concurrency")? {
        config.concurrency_limit = value as usize;
    }
    if let Some(value) = args.number("restore-concurrency")? {
        config.restore_concurrency = value as usize;
    }
    if let Some(value) = args.value("zip") {
        config.zip_mode = match value.to_lowercase().as_str() {
            "per-account" | "peraccount" | "cuenta" => ZipMode::PerAccount,
            "consolidated" | "consolidado" | "maestro" => ZipMode::Consolidated,
            "none" | "ninguno" | "sin" => ZipMode::None,
            other => usage_bail!("--zip no reconoce '{}'", other),
        };
    }
    if let Some(value) = args.value("format") {
        config.export_format = match value.to_lowercase().as_str() {
            "eml" => ExportFormat::Eml,
            "mbox" => ExportFormat::Mbox,
            "maildir" => ExportFormat::Maildir,
            other => usage_bail!("--format no reconoce '{}'", other),
        };
    }
    if let Some(value) = args.value("compression") {
        config.zip_compression = match value.to_lowercase().as_str() {
            "deflate" | "comprimir" => ZipCompression::Deflate,
            "store" | "sin-comprimir" => ZipCompression::Store,
            other => usage_bail!("--compression no reconoce '{}'", other),
        };
    }
    if args.has("cleanup-after-zip") {
        config.cleanup_raw_after_zip = true;
    }
    if let Some(var) = args.value("password-env") {
        for account in &mut config.accounts {
            account.password_env = Some(var.to_string());
        }
    }
    let only = args.values_of("account");
    if !only.is_empty() {
        config.accounts.retain(|account| {
            only.iter()
                .any(|email| email.eq_ignore_ascii_case(&account.email))
        });
        if config.accounts.is_empty() {
            usage_bail!("Ninguna de las cuentas indicadas con --account está en la configuración.");
        }
    }
    Ok(())
}

fn build_dest_conn(args: &Args, config: &AppConfig, secrets: &Secrets) -> Result<ConnOptions> {
    if let Some(email) = args.value("dest-account") {
        let account = config
            .account_by_email(email)
            .cloned()
            .usage(format!("La cuenta de destino '{}' no está en la configuración", email))?;
        return ConnOptions::from_account(&account, secrets).map(|conn| conn.with_timeout(config.timeout_secs));
    }

    let host = args
        .value("dest-host")
        .usage("Falta --dest-host o --dest-account para indicar el destino")?;
    let port = args
        .number("dest-port")?
        .map(|value| value as u16)
        .unwrap_or_else(|| {
            if args
                .value("dest-tls")
                .map(|mode| mode.eq_ignore_ascii_case("implicit"))
                .unwrap_or(true)
            {
                993
            } else {
                143
            }
        });
    let tls = match args.value("dest-tls") {
        Some(value) => match value.to_lowercase().as_str() {
            "implicit" | "ssl" | "tls" => TlsMode::Implicit,
            "starttls" => TlsMode::StartTls,
            "plain" | "none" | "sin" => TlsMode::Plain,
            other => usage_bail!("--dest-tls no reconoce '{}'", other),
        },
        None => TlsMode::Implicit,
    };
    let email = args
        .value("dest-email")
        .usage("Falta --dest-email con el usuario del destino")?;
    let password = match args.value("dest-password-env") {
        Some(var) => std::env::var(var)
            .usage(format!("La variable de entorno '{}' no está definida", var))?,
        None => crate::config::prompt_secret(&format!("Contraseña IMAP para {}: ", email))
            .map_err(|err| anyhow::Error::new(UsageError::new(format!("{:#}", err))))?,
    };

    Ok(migrate::dest_conn_options(
        host,
        port,
        tls,
        email,
        &password,
        config.timeout_secs,
    ))
}

fn test_connections(config: &AppConfig, secrets: &Secrets, ctx: &Ctx) -> Result<()> {
    let mut failures = 0usize;
    for account in &config.accounts {
        let scope = account.email.clone();
        match ConnOptions::from_account(account, secrets) {
            Ok(conn) => {
                let conn = conn.with_timeout(config.timeout_secs);
                ctx.info(&scope, format!("Probando {}", conn.describe()));
                match crate::connect::connect_and_login(&conn) {
                    Ok(mut session) => match crate::connect::list_folders(&mut session) {
                        Ok(folders) => {
                            ctx.success(
                                &scope,
                                format!("Conexión correcta: {} carpeta(s).", folders.len()),
                            );
                            for folder in &folders {
                                let marker = match folder.special_use {
                                    Some(use_kind) => format!(" [{}]", use_kind.label()),
                                    None => {
                                        if folder.selectable {
                                            String::new()
                                        } else {
                                            " [no seleccionable]".to_string()
                                        }
                                    }
                                };
                                println!(
                                    "    {} (delimitador '{}'){}",
                                    folder.name,
                                    folder.delimiter.unwrap_or('/'),
                                    marker
                                );
                            }
                            let _ = session.logout();
                        }
                        Err(err) => {
                            failures += 1;
                            ctx.error(&scope, format!("Autenticó pero LIST falló: {:#}", err));
                        }
                    },
                    Err(err) => {
                        failures += 1;
                        ctx.error(&scope, format!("Fallo de conexión: {:#}", err));
                    }
                }
            }
            Err(err) => {
                failures += 1;
                ctx.error(&scope, format!("Credenciales: {:#}", err));
            }
        }
    }

    if failures > 0 {
        bail!("{} cuenta(s) fallaron la prueba de conexión.", failures);
    }
    Ok(())
}

fn run_post_hooks(config: &AppConfig, report: &RunReport, ctx: &Ctx) {
    let success = !report.has_errors() && !report.cancelled;

    let command = match report.task.as_str() {
        task if task.starts_with("Restauración") => config.hooks.post_restore_command.clone(),
        _ => config.hooks.post_backup_command.clone(),
    };
    if let Some(command) = command {
        let env = vec![
            ("IMAPB_TASK", report.task.clone()),
            ("IMAPB_MESSAGES", report.totals.messages.to_string()),
            ("IMAPB_UPLOADED", report.totals.uploaded.to_string()),
            ("IMAPB_ERRORS", report.totals.errors.to_string()),
            ("IMAPB_OK", if success { "1".into() } else { "0".into() }),
        ];
        let refs: Vec<(&str, String)> = env;
        let _ = crate::hooks::run_shell_hook(ctx, &command, &refs);
    }

    let send_webhook = match config.hooks.failure_webhook_url {
        Some(_) => !success || config.hooks.webhook_on_success,
        None => false,
    };
    if send_webhook {
        if let Some(url) = &config.hooks.failure_webhook_url {
            let payload = serde_json::json!({
                "task": report.task,
                "ok": success,
                "cancelled": report.cancelled,
                "accounts": report.totals.accounts,
                "messages": report.totals.messages,
                "uploaded": report.totals.uploaded,
                "errors": report.totals.errors,
                "bytes": report.totals.bytes,
                "elapsed_secs": report.elapsed_secs,
                "summary": report.summary_line(),
            });
            crate::hooks::notify_webhook(ctx, url, &payload.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AccountConfig;

    fn parse(line: &str) -> Args {
        let mut argv = vec!["imap-backup".to_string()];
        argv.extend(line.split_whitespace().map(|token| token.to_string()));
        Args::parse(&argv).unwrap()
    }

    #[test]
    fn parses_commands_and_flags() {
        let args = parse("backup --dry-run --concurrency 5 -c /tmp/config.toml");
        assert_eq!(args.command, "backup");
        assert!(args.has("dry-run"));
        assert_eq!(args.value("concurrency"), Some("5"));
        assert_eq!(args.value("config"), Some("/tmp/config.toml"));
    }

    #[test]
    fn parses_inline_values_and_repeated_accounts() {
        let args = parse("backup --output=/tmp/out --account a@b.com --account c@d.com");
        assert_eq!(args.value("output"), Some("/tmp/out"));
        assert_eq!(args.values_of("account").len(), 2);
    }

    #[test]
    fn defaults_to_gui_without_arguments() {
        let argv = vec!["imap-backup".to_string()];
        assert_eq!(Args::parse(&argv).unwrap().command, "gui");
    }

    #[test]
    fn rejects_unknown_command_and_missing_value() {
        let argv = vec!["imap-backup".to_string(), "inventado".to_string()];
        assert!(Args::parse(&argv).is_err());
        let argv = vec!["imap-backup".to_string(), "--concurrency".to_string()];
        assert!(Args::parse(&argv).is_err());
    }

    #[test]
    fn short_flags_map_to_long_names() {
        let args = parse("backup -o /tmp/x -c cfg.toml");
        assert_eq!(args.value("output"), Some("/tmp/x"));
        assert_eq!(args.value("config"), Some("cfg.toml"));
    }

    #[test]
    fn overrides_config_from_flags() {
        let mut config = AppConfig::default();
        let args = parse("backup --output /tmp/dest --zip consolidated --format mbox --compression store --cleanup-after-zip");
        apply_overrides(&mut config, &args).unwrap();
        assert_eq!(config.output_dir, PathBuf::from("/tmp/dest"));
        assert_eq!(config.zip_mode, ZipMode::Consolidated);
        assert_eq!(config.export_format, ExportFormat::Mbox);
        assert_eq!(config.zip_compression, ZipCompression::Store);
        assert!(config.cleanup_raw_after_zip);
    }

    #[test]
    fn account_filter_keeps_only_requested_accounts() {
        let mut config = AppConfig::default();
        config.accounts.push(AccountConfig::new("a@b.com", "h"));
        config.accounts.push(AccountConfig::new("c@d.com", "h"));
        let args = parse("backup --account c@d.com");
        apply_overrides(&mut config, &args).unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].email, "c@d.com");
    }

    #[test]
    fn rejects_unknown_flag_values() {
        let mut config = AppConfig::default();
        let args = parse("backup --zip raro");
        assert!(apply_overrides(&mut config, &args).is_err());
    }

    #[test]
    fn dest_conn_requires_host_and_email() {
        let config = AppConfig::default();
        let secrets = Secrets::default();
        let args = parse("restore --source /tmp/x.zip");
        assert!(build_dest_conn(&args, &config, &secrets).is_err());
    }

    #[test]
    fn dest_conn_from_config_account_reuses_credentials() {
        let mut config = AppConfig::default();
        let mut account = AccountConfig::new("dest@b.com", "imap.b.com");
        account.password = Some("pwd".to_string());
        config.accounts.push(account);
        let secrets = Secrets::default();
        let args = parse("restore --dest-account dest@b.com --source /tmp/x.zip");
        let conn = build_dest_conn(&args, &config, &secrets).unwrap();
        assert_eq!(conn.host, "imap.b.com");
        assert_eq!(conn.password.as_deref(), Some("pwd"));
    }

    #[test]
    fn help_mentions_every_command() {
        for command in [
            "backup",
            "restore",
            "migrate",
            "test-connection",
            "verify",
            "extract",
            "gui",
            "help",
        ] {
            assert!(HELP.contains(command), "la ayuda no menciona {}", command);
        }
    }

    #[test]
    fn unknown_tls_mode_is_rejected() {
        let mut argv = vec!["imap-backup".to_string(), "restore".to_string()];
        argv.extend(
            ["--dest-host", "h", "--dest-email", "a@b.com", "--dest-tls", "raro"]
                .iter()
                .map(|token| token.to_string()),
        );
        let args = Args::parse(&argv).unwrap();
        let config = AppConfig::default();
        let secrets = Secrets::default();
        assert!(build_dest_conn(&args, &config, &secrets).is_err());
    }
}
