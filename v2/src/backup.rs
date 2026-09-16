// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Motor de respaldo.
//!
//! Flujo por cuenta:
//! 1. `LIST` de buzones (con `\Noselect` y carpetas especiales).
//! 2. Por carpeta: `EXAMINE` (nunca modifica el buzón) para obtener `EXISTS`,
//!    `UIDVALIDITY` y `UIDNEXT`.
//! 3. Se decide qué metadatos pedir (nada / desde UID / todo) y qué mensajes bajar
//!    (`plan_sync`), sin traer cuerpos que ya están en disco.
//! 4. Descarga por lotes con presupuesto de bytes, escritura atómica y manifiesto.
//! 5. Verificación contra el conteo del servidor, ZIP opcional y reporte.
//!
//! Todo el hilo de trabajo comparte una sola sesión IMAP por cuenta: la v1 abría y
//! cerraba una conexión TLS cada 25 mensajes, lo que en un buzón grande significa
//! cientos de handshakes y de `LOGIN` (justo lo que dispara los rate-limits).

use crate::archive::{self, ZipStats};
use crate::config::{AccountConfig, AppConfig, ExportFormat, Secrets, ZipMode};
use crate::connect::{self, ConnOptions, SessionHolder};
use crate::events::{AccountRunState, Ctx, TaskEvent, TaskKind};
use crate::folders::{self, SpecialUse};
use crate::formats::{maildir::MaildirWriter, mbox::MboxWriter};
use crate::manifest::{self, AccountManifest, FolderManifest, MessageEntry, ServerMessage};
use crate::report::{AccountReport, FolderReport, RunReport};
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROGRESS_EVERY_MESSAGES: u64 = 20;
const PROGRESS_EVERY_SECS: f64 = 0.25;
const MANIFEST_SAVE_EVERY_MESSAGES: u64 = 200;
const MANIFEST_SAVE_EVERY_SECS: f64 = 10.0;

#[derive(Debug, Clone)]
pub struct BackupOptions {
    pub output_dir: PathBuf,
    pub concurrency: usize,
    pub dry_run: bool,
    pub force_full: bool,
    pub verify: bool,
}

impl BackupOptions {
    pub fn from_config(cfg: &AppConfig) -> Self {
        Self {
            output_dir: cfg.output_dir.clone(),
            concurrency: cfg.concurrency_limit.max(1),
            dry_run: false,
            force_full: false,
            verify: cfg.verify_after_backup,
        }
    }
}

/// Ejecuta el respaldo de todas las cuentas configuradas.
pub fn run(
    cfg: &AppConfig,
    secrets: &Secrets,
    ctx: &Ctx,
    opts: &BackupOptions,
) -> Result<RunReport> {
    let started = Instant::now();
    let mut report = RunReport::new(if opts.dry_run { "Backup (simulación)" } else { "Backup" });
    report.free_space_before = crate::fsutil::free_space(&opts.output_dir);

    if cfg.accounts.is_empty() {
        anyhow::bail!("No hay cuentas configuradas para respaldar.");
    }

    ctx.send(TaskEvent::Started {
        kind: TaskKind::Backup,
        detail: format!(
            "{} cuenta(s) en {}",
            cfg.accounts.len(),
            opts.output_dir.display()
        ),
    });
    ctx.info(
        "backup",
        format!(
            "=== INICIANDO RESPALDO: {} cuenta(s) → {} (concurrencia {}, formato {}, ZIP {}){} ===",
            cfg.accounts.len(),
            opts.output_dir.display(),
            opts.concurrency,
            cfg.export_format.label(),
            cfg.zip_mode.label(),
            if opts.dry_run { " [SIMULACIÓN]" } else { "" }
        ),
    );
    for warning in cfg.warnings() {
        ctx.warn("config", warning);
    }

    let queue: Arc<Mutex<VecDeque<AccountConfig>>> =
        Arc::new(Mutex::new(cfg.accounts.iter().cloned().collect()));
    let results: Arc<Mutex<Vec<AccountReport>>> = Arc::new(Mutex::new(Vec::new()));
    let workers = opts.concurrency.min(cfg.accounts.len()).max(1);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let results = Arc::clone(&results);
            scope.spawn(move || loop {
                let next = { queue.lock().expect("cola de cuentas").pop_front() };
                let Some(account) = next else { break };
                if ctx.cancelled() {
                    break;
                }
                ctx.send(TaskEvent::Account {
                    account: account.email.clone(),
                    state: AccountRunState::Running {
                        folder: String::new(),
                        current: 0,
                        total: 0,
                        bytes: 0,
                    },
                });
                let account_report = backup_account(&account, cfg, secrets, ctx, opts);
                ctx.send(TaskEvent::Account {
                    account: account.email.clone(),
                    state: AccountRunState::Finished(Box::new(account_report.clone())),
                });
                results.lock().expect("resultados").push(account_report);
            });
        }
    });

    let mut accounts = Arc::try_unwrap(results)
        .map(|mutex| mutex.into_inner().expect("resultados"))
        .unwrap_or_else(|shared| shared.lock().expect("resultados").clone());
    accounts.sort_by(|a, b| a.account.cmp(&b.account));
    report.accounts = accounts;
    report.estimated_bytes = Some(report.accounts.iter().map(|a| a.bytes).sum());

    // Aviso preventivo de espacio en disco antes de que sea tarde.
    if let (Some(free), Some(planned)) = (report.free_space_before, report.estimated_bytes) {
        if planned > free / 2 && planned > 0 {
            ctx.warn(
                "backup",
                format!(
                    "Espacio libre en destino: {}. Este respaldo escribió {}.",
                    crate::fsutil::human_bytes(free),
                    crate::fsutil::human_bytes(planned)
                ),
            );
        }
    }

    if cfg.zip_mode == ZipMode::Consolidated && !opts.dry_run {
        consolidated_zip(cfg, ctx)?;
    }

    report.finish(started.elapsed().as_secs_f64(), ctx.cancelled());

    if ctx.cancelled() {
        ctx.warn(
            "backup",
            "Respaldo cancelado por el usuario. El manifiesto quedó guardado: volvé a ejecutarlo para continuar donde quedó.",
        );
    } else if report.has_errors() {
        ctx.warn("backup", report.summary_line());
    } else {
        ctx.success("backup", report.summary_line());
    }

    ctx.send(TaskEvent::Finished {
        summary: Box::new(report.clone()),
    });
    Ok(report)
}

/// Respalda una cuenta completa y devuelve su reporte.
pub fn backup_account(
    account: &AccountConfig,
    cfg: &AppConfig,
    secrets: &Secrets,
    ctx: &Ctx,
    opts: &BackupOptions,
) -> AccountReport {
    let started = Instant::now();
    let label = account.get_domain_or_label();
    let mut report = AccountReport::new(account.email.clone(), account.host.clone());
    let scope = account.email.clone();

    let conn = match ConnOptions::from_account(account, secrets)
        .map(|opts| opts.with_timeout(cfg.timeout_secs))
    {
        Ok(conn) => conn,
        Err(err) => {
            report.errors += 1;
            report.error_list.push(err.to_string());
            ctx.error(&scope, format!("Credenciales incompletas: {}", err));
            report.elapsed_secs = started.elapsed().as_secs_f64();
            return report;
        }
    };
    ctx.info(&scope, format!("Conectando a {}", conn.describe()));

    let account_dir = folders::account_dir(&opts.output_dir, &label, &account.email);
    if !opts.dry_run {
        if let Err(err) = std::fs::create_dir_all(&account_dir) {
            report.errors += 1;
            report.error_list.push(err.to_string());
            ctx.error(
                &scope,
                format!("No se pudo crear {}: {}", account_dir.display(), err),
            );
            report.elapsed_secs = started.elapsed().as_secs_f64();
            return report;
        }
    }

    let mut manifest = AccountManifest::load(&account_dir)
        .unwrap_or_else(|| AccountManifest::new(&account.email, &account.host, "Eml"));
    if opts.force_full {
        ctx.warn(
            &scope,
            "Resincronización completa solicitada: se ignorará el manifiesto previo.",
        );
        manifest.folders.clear();
    }
    manifest.format = match cfg.export_format {
        ExportFormat::Eml => "Eml",
        ExportFormat::Mbox => "Mbox",
        ExportFormat::Maildir => "Maildir",
    }
    .to_string();

    let mut holder = SessionHolder::new(conn, account.email.clone());

    let remote_folders = match connect::with_retry(
        ctx,
        &mut holder,
        "Listado de carpetas (LIST)",
        cfg.retry_attempts,
        cfg.retry_backoff_base_secs,
        cfg.retry_backoff_max_secs,
        connect::list_folders,
    ) {
        Ok(folders) => folders,
        Err(err) => {
            report.errors += 1;
            report.error_list.push(err.to_string());
            report.elapsed_secs = started.elapsed().as_secs_f64();
            return report;
        }
    };

    let mut selected: Vec<connect::RemoteFolder> = Vec::new();
    for folder in remote_folders {
        if !folder.selectable {
            ctx.debug(&scope, format!("Carpeta omitida (\\Noselect): {}", folder.name));
            continue;
        }
        if !folders::matches_filter(
            &folder.name,
            &account.exclude_folders,
            account.include_only_folders.as_deref(),
        ) {
            ctx.debug(&scope, format!("Carpeta excluida por configuración: {}", folder.name));
            continue;
        }
        if cfg.skip_all_mail && folder.special_use == Some(SpecialUse::All) {
            ctx.info(
                &scope,
                format!(
                    "Carpeta '{}' omitida: es un buzón virtual \\All y sus mensajes ya están en las demás carpetas.",
                    folder.name
                ),
            );
            continue;
        }
        selected.push(folder);
    }

    if selected.is_empty() {
        report.errors += 1;
        let message = "No quedó ninguna carpeta para respaldar (revisá exclude_folders).";
        report.error_list.push(message.to_string());
        ctx.error(&scope, message);
        report.elapsed_secs = started.elapsed().as_secs_f64();
        return report;
    }

    ctx.info(
        &scope,
        format!("{} carpeta(s) a procesar.", selected.len()),
    );

    let mut saved_at = Instant::now();
    let mut pending_saves: u64 = 0;

    for (index, folder) in selected.iter().enumerate() {
        if let Err(err) = ctx.check_cancel() {
            ctx.warn(&scope, format!("Cancelado antes de '{}': {}", folder.name, err));
            let _ = manifest.save(&account_dir);
            break;
        }

        ctx.send(TaskEvent::Account {
            account: account.email.clone(),
            state: AccountRunState::Running {
                folder: folder.name.clone(),
                current: 0,
                total: 0,
                bytes: 0,
            },
        });

        let mut folder_report = FolderReport {
            folder: folder.name.clone(),
            remote_name: Some(folder.name.clone()),
            ..Default::default()
        };

        match backup_folder(
            account,
            folder,
            cfg,
            opts,
            ctx,
            &mut holder,
            &account_dir,
            &mut manifest,
            &mut folder_report,
        ) {
            Ok(()) => {
                ctx.info(
                    &scope,
                    format!(
                        "[{}/{}] '{}' listo: {} mensaje(s), {} nuevo(s), {} omitido(s), {} reparado(s), {}{}",
                        index + 1,
                        selected.len(),
                        folder.name,
                        folder_report.messages,
                        folder_report.new,
                        folder_report.skipped,
                        folder_report.repaired,
                        crate::fsutil::human_bytes(folder_report.bytes),
                        if folder_report.verified { ", verificado" } else { "" }
                    ),
                );
            }
            Err(err) => {
                if crate::cancel::is_cancelled(&err) {
                    ctx.warn(&scope, format!("Cancelado en '{}'.", folder.name));
                    folder_report.errors += 1;
                    folder_report.errors_detail.push(err.to_string());
                } else {
                    ctx.error(&scope, format!("Error en '{}': {:#}", folder.name, err));
                    folder_report.errors += 1;
                    folder_report.errors_detail.push(format!("{:#}", err));
                }
            }
        }

        pending_saves += folder_report.new;
        report.folders.push(folder_report);

        if !opts.dry_run
            && (pending_saves >= MANIFEST_SAVE_EVERY_MESSAGES
                || saved_at.elapsed().as_secs_f64() >= MANIFEST_SAVE_EVERY_SECS)
        {
            if let Err(err) = manifest.save(&account_dir) {
                ctx.warn(&scope, format!("No se pudo guardar el manifiesto: {}", err));
            }
            saved_at = Instant::now();
            pending_saves = 0;
        }
    }

    if !opts.dry_run {
        if let Err(err) = manifest.save(&account_dir) {
            ctx.warn(&scope, format!("No se pudo guardar el manifiesto final: {}", err));
        }
    }

    holder.disconnect();
    report.recompute();

    // ZIP por cuenta: se genera aunque haya errores parciales (la v1 lo omitía por
    // completo si un solo mensaje fallaba, dejando la cuenta sin archivo).
    if cfg.zip_mode == ZipMode::PerAccount && !opts.dry_run {
        let files = crate::fsutil::count_files_with_ext(&account_dir, "eml");
        if files > 0 || !report.folders.is_empty() {
            let zip_path = archive::account_zip_path(&opts.output_dir, &label, &account.email);
            let compression = cfg.zip_compression;
            let zipped = archive::zip_directory(&account_dir, &zip_path, compression, &mut |files, bytes| {
                if files % 50 == 0 {
                    ctx.debug(
                        &scope,
                        format!("Comprimiendo: {} archivo(s), {}", files, crate::fsutil::human_bytes(bytes)),
                    );
                }
            });
            match zipped {
                Ok(stats) => {
                    ctx.success(
                        &scope,
                        format!(
                            "ZIP generado: {} ({} archivo(s), {})",
                            zip_path.display(),
                            stats.files,
                            crate::fsutil::human_bytes(stats.output_bytes)
                        ),
                    );
                    report.zip_paths.push(zip_path.display().to_string());
                    if cfg.cleanup_raw_after_zip {
                        if let Err(err) = std::fs::remove_dir_all(&account_dir) {
                            ctx.warn(&scope, format!("No se pudo limpiar {}: {}", account_dir.display(), err));
                        } else {
                            ctx.info(&scope, "Carpetas .eml originales eliminadas tras el ZIP.");
                        }
                    }
                }
                Err(err) => {
                    ctx.error(&scope, format!("Error generando el ZIP: {:#}", err));
                    report.errors += 1;
                    report.error_list.push(format!("ZIP: {:#}", err));
                }
            }
        }
    }

    report.elapsed_secs = started.elapsed().as_secs_f64();
    report.recompute();

    if report.errors == 0 && !opts.dry_run {
        ctx.success(
            &scope,
            format!(
                "Cuenta completada: {} mensaje(s), {} nuevo(s), {} omitido(s), {} en {}{}",
                report.messages_total,
                report.new,
                report.skipped,
                crate::fsutil::human_bytes(report.bytes),
                crate::fsutil::human_duration(report.elapsed_secs),
                if report.verified { ", verificado contra el servidor" } else { "" }
            ),
        );
    } else if !opts.dry_run {
        ctx.warn(
            &scope,
            format!(
                "Cuenta finalizada con {} error(es) ({} mensaje(s) nuevos, {} omitidos)",
                report.errors, report.new, report.skipped
            ),
        );
    }

    report
}

/// Procesa una carpeta: planifica, descarga y verifica.
#[allow(clippy::too_many_arguments)]
fn backup_folder(
    account: &AccountConfig,
    folder: &connect::RemoteFolder,
    cfg: &AppConfig,
    opts: &BackupOptions,
    ctx: &Ctx,
    holder: &mut SessionHolder,
    account_dir: &Path,
    manifest: &mut AccountManifest,
    report: &mut FolderReport,
) -> Result<()> {
    let scope = account.email.clone();
    let folder_name = folder.name.clone();

    // 1. SELECT en modo lectura (examine): nunca marca mensajes como leídos.
    let info = connect::with_retry(
        ctx,
        holder,
        &format!("Seleccionar carpeta '{}'", folder_name),
        cfg.retry_attempts,
        cfg.retry_backoff_base_secs,
        cfg.retry_backoff_max_secs,
        |session| connect::select_mailbox(session, &folder_name, true),
    )?;

    report.server_count = Some(info.exists);

    let prev: Option<FolderManifest> = manifest.folder(&folder_name).cloned();
    let metadata_plan = manifest::plan_metadata_fetch(prev.as_ref(), info.uid_validity, info.uid_next);

    let server_messages: Vec<ServerMessage> = match metadata_plan {
        // Nada cambió: se reusa el manifiesto, sin tráfico de red. Igual se valida
        // que los archivos sigan en disco (un backup borrado se detecta y se repara).
        manifest::MetadataFetch::Nothing => prev
            .as_ref()
            .map(|folder_manifest| {
                folder_manifest
                    .messages
                    .iter()
                    .map(|entry| ServerMessage {
                        uid: entry.uid,
                        size: entry.size,
                        message_id: entry.message_id.clone(),
                        internal_date: entry.internal_date.clone(),
                        flags: entry.flags.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        manifest::MetadataFetch::Since(uid) => {
            let range = format!("{}:*", uid);
            ctx.debug(
                &scope,
                format!("'{}': pidiendo metadatos nuevos desde UID {}", folder_name, uid),
            );
            connect::with_retry(
                ctx,
                holder,
                &format!("Metadatos de '{}'", folder_name),
                cfg.retry_attempts,
                cfg.retry_backoff_base_secs,
                cfg.retry_backoff_max_secs,
                |session| connect::fetch_metadata(session, &range),
            )?
        }
        manifest::MetadataFetch::All => connect::with_retry(
            ctx,
            holder,
            &format!("Metadatos de '{}'", folder_name),
            cfg.retry_attempts,
            cfg.retry_backoff_base_secs,
            cfg.retry_backoff_max_secs,
            |session| connect::fetch_metadata(session, "1:*"),
        )?,
    };

    // En mbox varios mensajes comparten archivo: no se puede validar por tamaño.
    let check_file_size = cfg.export_format != ExportFormat::Mbox;
    let plan = manifest::plan_sync(
        &server_messages,
        prev.as_ref(),
        info.uid_validity,
        account_dir,
        cfg.skip_existing,
        check_file_size,
    );
    report.repaired = plan.repaired;

    if plan.invalidated {
        ctx.warn(
            &scope,
            format!(
                "'{}': cambió UIDVALIDITY ({} → {}): se resincroniza completo porque los UIDs anteriores ya no son válidos.",
                folder_name,
                prev.as_ref().map(|p| p.uid_validity).unwrap_or(0),
                info.uid_validity
            ),
        );
        report.invalidated = true;
        manifest.folders.remove(&folder_name);
    }
    if plan.repaired > 0 {
        ctx.warn(
            &scope,
            format!(
                "'{}': {} archivo(s) local(es) faltaban o estaban incompletos: se vuelven a descargar.",
                folder_name, plan.repaired
            ),
        );
    }
    if plan.orphans > 0 {
        ctx.info(
            &scope,
            format!(
                "'{}': {} mensaje(s) del manifiesto ya no existen en el servidor (se conservan localmente).",
                folder_name, plan.orphans
            ),
        );
    }

    report.messages = plan.total_messages;
    report.skipped = plan.unchanged;
    report.local_count = if plan.needs_download() {
        plan.unchanged
    } else {
        plan.total_messages
    };

    // 2. Actualización de flags sin volver a descargar (solo .eml, donde los flags
    //    viven en el manifiesto y no en el nombre del archivo).
    if cfg.export_format == ExportFormat::Eml && !plan.flag_updates.is_empty() {
        let folder_manifest = manifest.folder_mut(&folder_name);
        for entry in &plan.flag_updates {
            folder_manifest.upsert(entry.clone());
        }
        ctx.debug(
            &scope,
            format!(
                "'{}': {} mensaje(s) con flags actualizados en el manifiesto.",
                folder_name,
                plan.flag_updates.len()
            ),
        );
    }

    if opts.dry_run {
        ctx.info(
            &scope,
            format!(
                "[simulación] '{}': se descargarían {} mensaje(s) ({}) de {} en el servidor.",
                folder_name,
                plan.to_download.len(),
                crate::fsutil::human_bytes(plan.download_bytes),
                plan.total_messages
            ),
        );
        report.verified = true;
        report.local_count = plan.total_messages;
        return Ok(());
    }

    // 3. Preparar el destino según el formato elegido.
    let relative = folders::folder_relpath(&folder_name, folder.delimiter);
    let folder_dir = account_dir.join(&relative);
    let mut sink = FolderSink::open(cfg.export_format, account_dir, &folder_dir)?;

    // 4. Descargar por lotes acotados por bytes.
    let total_to_download = plan.to_download.len() as u64;
    let mut downloaded: u64 = 0;
    let mut downloaded_bytes: u64 = 0;
    let mut batch: Vec<ServerMessage> = Vec::new();
    let mut batch_bytes: u64 = 0;
    let mut last_progress = Instant::now();

    let flush = |ctx: &Ctx,
                 holder: &mut SessionHolder,
                     sink: &mut FolderSink,
                     manifest: &mut AccountManifest,
                     report: &mut FolderReport,
                     batch: &mut Vec<ServerMessage>,
                     batch_bytes: &mut u64,
                     downloaded: &mut u64,
                     downloaded_bytes: &mut u64,
                     last_progress: &mut Instant,
                     force: bool|
     -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let budget_reached = *batch_bytes >= cfg.chunk_bytes || batch.len() >= cfg.max_chunk_messages;
        if !force && !budget_reached {
            return Ok(());
        }

        let uid_set = manifest::seq_set(&batch.iter().map(|m| m.uid).collect::<Vec<_>>());
        let fetched = connect::with_retry(
            ctx,
            holder,
            &format!("Descarga de {} mensaje(s) de '{}'", batch.len(), folder_name),
            cfg.retry_attempts,
            cfg.retry_backoff_base_secs,
            cfg.retry_backoff_max_secs,
            |session| connect::fetch_bodies(session, &uid_set),
        )?;

        let by_uid: std::collections::HashMap<u32, &ServerMessage> =
            batch.iter().map(|m| (m.uid, m)).collect();

        for message in fetched {
            let Some(planned) = by_uid.get(&message.uid) else {
                continue;
            };
            let Some(body) = message.body.as_ref() else {
                report.errors += 1;
                report.errors_detail.push(format!(
                    "UID {} sin cuerpo RFC822 (el servidor no devolvió BODY[])",
                    message.uid
                ));
                *downloaded += 1;
                continue;
            };

            // El tamaño anunciado debe coincidir: si no, se asume descarga parcial
            // y se reintenta en la próxima corrida en vez de guardar basura.
            if planned.size > 0 && body.len() as u64 != planned.size {
                report.errors += 1;
                report.errors_detail.push(format!(
                    "UID {}: el servidor anunció {} bytes y devolvió {} (no se guarda)",
                    message.uid,
                    planned.size,
                    body.len()
                ));
                *downloaded += 1;
                continue;
            }

            let internal_date = message
                .internal_date
                .clone()
                .or_else(|| planned.internal_date.clone());
            let message_id = message
                .message_id
                .clone()
                .or_else(|| planned.message_id.clone())
                .or_else(|| crate::formats::maildir::message_id_of(body));
            let flags = if message.flags.is_empty() {
                planned.flags.clone()
            } else {
                message.flags.clone()
            };

            let from_address = crate::formats::header_value(body, "From")
                .map(|value| crate::formats::address_of(&value))
                .unwrap_or_else(|| account.email.clone());

            let stored = sink.store(
                account_dir,
                &folder_name,
                &folder_dir,
                message.uid,
                body,
                message_id.as_deref(),
                &flags,
                internal_date.as_deref(),
                &from_address,
            )?;

            let sha = if cfg.hash_verify {
                Some(manifest::sha256_hex(body))
            } else {
                None
            };

            manifest.folder_mut(&folder_name).upsert(MessageEntry {
                uid: message.uid,
                message_id,
                size: body.len() as u64,
                internal_date,
                flags,
                file: stored,
                sha256: sha,
            });

            report.new += 1;
            report.bytes += body.len() as u64;
            *downloaded += 1;
            *downloaded_bytes += body.len() as u64;
        }

        batch.clear();
        *batch_bytes = 0;

        if last_progress.elapsed().as_secs_f64() >= PROGRESS_EVERY_SECS
            || (*downloaded).is_multiple_of(PROGRESS_EVERY_MESSAGES)
            || *downloaded >= total_to_download
        {
            ctx.send(TaskEvent::Account {
                account: account.email.clone(),
                state: AccountRunState::Running {
                    folder: folder_name.clone(),
                    current: *downloaded,
                    total: total_to_download,
                    bytes: *downloaded_bytes,
                },
            });
            *last_progress = Instant::now();
        }
        Ok(())
    };

    for message in &plan.to_download {
        ctx.check_cancel()?;
        batch_bytes = batch_bytes.saturating_add(message.size);
        batch.push(message.clone());
        flush(
            ctx,
            holder,
            &mut sink,
            manifest,
            report,
            &mut batch,
            &mut batch_bytes,
            &mut downloaded,
            &mut downloaded_bytes,
            &mut last_progress,
            false,
        )?;
    }
    flush(
        ctx,
        holder,
        &mut sink,
        manifest,
        report,
        &mut batch,
        &mut batch_bytes,
        &mut downloaded,
        &mut downloaded_bytes,
        &mut last_progress,
        true,
    )?;

    sink.finish()?;

    // 5. Estado del manifiesto para esta carpeta.
    {
        let folder_manifest = manifest.folder_mut(&folder_name);
        folder_manifest.delimiter = folder.delimiter;
        folder_manifest.special_use = folder.special_use.map(|s| s.label().to_string());
        folder_manifest.uid_validity = info.uid_validity;
        folder_manifest.uid_next = info.uid_next;
        folder_manifest.server_count = info.exists;
        folder_manifest.run_id = chrono::Local::now().to_rfc3339();
    }

    // 6. Verificación: el conteo local debe coincidir con el del servidor.
    let local = manifest.folder_message_count(&folder_name);
    report.local_count = local;
    if opts.verify {
        report.verified = local == info.exists;
        if !report.verified {
            ctx.warn(
                &scope,
                format!(
                    "'{}': verificación con diferencias: servidor {} mensaje(s), local {}.",
                    folder_name, info.exists, local
                ),
            );
        }
    }

    Ok(())
}

/// Destino de escritura de los mensajes de una carpeta.
/// También lo usa la migración servidor→servidor cuando se pide una copia local.
pub(crate) enum FolderSink {
    Eml(PathBuf),
    Mbox(Box<MboxWriter>),
    Maildir(Box<MaildirWriter>),
}

impl FolderSink {
    pub(crate) fn open(
        format: ExportFormat,
        account_dir: &Path,
        folder_dir: &Path,
    ) -> Result<Self> {
        match format {
            ExportFormat::Eml => {
                std::fs::create_dir_all(folder_dir).with_context(|| {
                    format!("No se pudo crear {}", folder_dir.display())
                })?;
                Ok(FolderSink::Eml(folder_dir.to_path_buf()))
            }
            ExportFormat::Mbox => {
                // `INBOX/Sub` → `<cuenta>/INBOX/Sub.mbox`, lo que permite recuperar el
                // nombre remoto de forma determinista al restaurar.
                let relative = folder_dir
                    .strip_prefix(account_dir)
                    .unwrap_or(folder_dir)
                    .to_path_buf();
                let mut path = account_dir.join(relative);
                path.set_extension("mbox");
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                let writer = MboxWriter::create(&path)?;
                Ok(FolderSink::Mbox(Box::new(writer)))
            }
            ExportFormat::Maildir => {
                let writer = MaildirWriter::open(folder_dir)?;
                Ok(FolderSink::Maildir(Box::new(writer)))
            }
        }
    }

    /// Guarda un mensaje y devuelve su ruta relativa al directorio de la cuenta.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn store(
        &mut self,
        account_dir: &Path,
        _folder_name: &str,
        folder_dir: &Path,
        uid: u32,
        body: &[u8],
        message_id: Option<&str>,
        flags: &[String],
        internal_date: Option<&str>,
        from_address: &str,
    ) -> Result<String> {
        match self {
            FolderSink::Eml(dir) => {
                let path = dir.join(folders::eml_filename(uid, message_id));
                crate::fsutil::write_atomic(&path, body)?;
                Ok(relative_string(account_dir, &path))
            }
            FolderSink::Mbox(writer) => {
                let date = internal_date
                    .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
                    .map(|d| d.to_rfc2822());
                writer.append(body, from_address, date.as_deref())?;
                Ok(relative_string(account_dir, writer.path()))
            }
            FolderSink::Maildir(writer) => {
                let path = writer.append(uid, body, flags)?;
                let _ = folder_dir;
                Ok(relative_string(account_dir, &path))
            }
        }
    }

    pub(crate) fn finish(self) -> Result<()> {
        match self {
            FolderSink::Eml(_) => Ok(()),
            FolderSink::Mbox(writer) => {
                writer.finish()?;
                Ok(())
            }
            FolderSink::Maildir(_) => Ok(()),
        }
    }
}

fn relative_string(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// ZIP maestro con todas las cuentas.
fn consolidated_zip(cfg: &AppConfig, ctx: &Ctx) -> Result<ZipStats> {
    let zip_path = archive::consolidated_zip_path(&cfg.output_dir);
    ctx.info(
        "backup",
        format!("Generando ZIP consolidado en {}...", zip_path.display()),
    );
    let compression = cfg.zip_compression;
    let mut last_log = Instant::now();
    let stats = archive::zip_directory(&cfg.output_dir, &zip_path, compression, &mut |files, bytes| {
        if last_log.elapsed().as_secs_f64() >= 2.0 {
            last_log = Instant::now();
            ctx.debug(
                "backup",
                format!(
                    "Comprimiendo: {} archivo(s), {}",
                    files,
                    crate::fsutil::human_bytes(bytes)
                ),
            );
        }
    })?;

    ctx.success(
        "backup",
        format!(
            "ZIP consolidado: {} ({} archivo(s), {}, en disco {})",
            zip_path.display(),
            stats.files,
            crate::fsutil::human_bytes(stats.bytes),
            crate::fsutil::human_bytes(stats.output_bytes)
        ),
    );

    if cfg.cleanup_raw_after_zip {
        for account in &cfg.accounts {
            let domain_dir = cfg
                .output_dir
                .join(folders::sanitize_component(&account.get_domain_or_label()));
            if domain_dir.exists() {
                if let Err(err) = std::fs::remove_dir_all(&domain_dir) {
                    ctx.warn(
                        "backup",
                        format!("No se pudo limpiar {}: {}", domain_dir.display(), err),
                    );
                }
            }
        }
        ctx.info("backup", "Carpetas .eml originales eliminadas tras el ZIP consolidado.");
    }

    Ok(stats)
}

/// Utilidad para el modo headless: espera un poco entre reintentos de cuentas.
pub fn account_pause() -> Duration {
    Duration::from_millis(50)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_use_forward_slashes() {
        let base = Path::new("/tmp/cuenta");
        let path = base.join("INBOX").join("1_a.eml");
        assert_eq!(relative_string(base, &path), "INBOX/1_a.eml");
    }

    #[test]
    fn eml_sink_writes_only_after_full_body_is_known() {
        let dir = std::env::temp_dir().join(format!("imapb-sink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let account_dir = dir.join("dominio").join("a@b.com");
        let folder_dir = account_dir.join("INBOX");
        let mut sink = FolderSink::open(ExportFormat::Eml, &account_dir, &folder_dir).unwrap();
        sink.store(
            &account_dir,
            "INBOX",
            &folder_dir,
            3,
            b"Subject: X\r\n\r\ncuerpo",
            Some("<id@x>"),
            &["\\Seen".to_string()],
            Some("2024-01-02T03:04:05+00:00"),
            "a@b.com",
        )
        .unwrap();
        sink.finish().unwrap();
        let written = std::fs::read_dir(&folder_dir).unwrap().count();
        assert_eq!(written, 1);
        assert!(folder_dir.join("3_id@x.eml").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mbox_sink_names_file_after_folder_path() {
        let dir = std::env::temp_dir().join(format!("imapb-sink-mbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let account_dir = dir.join("a@b.com");
        let folder_dir = account_dir.join("INBOX").join("Sub");
        let mut sink = FolderSink::open(ExportFormat::Mbox, &account_dir, &folder_dir).unwrap();
        sink.store(
            &account_dir,
            "INBOX/Sub",
            &folder_dir,
            1,
            b"Subject: X\r\n\r\ncuerpo",
            Some("<id@x>"),
            &[],
            None,
            "a@b.com",
        )
        .unwrap();
        sink.finish().unwrap();
        let file = account_dir.join("INBOX").join("Sub.mbox");
        assert!(file.exists(), "falta {}", file.display());
        assert_eq!(
            folders::remote_name_from_relpath(Path::new("INBOX/Sub.mbox"), None),
            "INBOX/Sub"
        );
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.starts_with("From a@b.com "));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
