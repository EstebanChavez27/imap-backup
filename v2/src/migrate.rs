// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Migración directa servidor→servidor.
//!
//! Lee los mensajes del IMAP de origen y los inyecta en el IMAP de destino por
//! streaming: nada de ZIP ni de round-trip por disco. Cada unidad de trabajo es un
//! rango de UIDs de una carpeta, así que una única carpeta enorme también se
//! paraleliza (algo que la v1 no podía hacer en ningún escenario).
//!
//! Opcionalmente escribe una copia local (`.eml`, `mbox` o Maildir) para dejar
//! evidencia y permitir reanudar sin volver a pedirle todo al origen.

use crate::backup::FolderSink;
use crate::config::{AccountConfig, AppConfig, ExportFormat, TlsMode};
use crate::connect::{self, ConnOptions, SessionHolder};
use crate::events::{AccountRunState, Ctx, TaskEvent, TaskKind};
use crate::folders::{self, SpecialUse};
use crate::report::{AccountReport, FolderReport, RunReport};
use anyhow::Result;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const UID_RANGE_SIZE: usize = 500;

#[derive(Debug, Clone)]
pub struct MigrateOptions {
    pub source: AccountConfig,
    pub source_conn: ConnOptions,
    pub dest_conn: ConnOptions,
    pub concurrency: usize,
    pub skip_existing: bool,
    pub dry_run: bool,
    pub folder_prefix: Option<String>,
    /// Directorio donde dejar una copia local de lo migrado (opcional).
    pub keep_local: Option<PathBuf>,
    pub keep_local_format: ExportFormat,
    pub retry_attempts: u32,
    pub backoff_base_secs: u64,
    pub backoff_max_secs: u64,
    pub verify: bool,
    pub skip_all_mail: bool,
}

impl MigrateOptions {
    pub fn new(source: AccountConfig, source_conn: ConnOptions, dest_conn: ConnOptions) -> Self {
        Self {
            source,
            source_conn,
            dest_conn,
            concurrency: 2,
            skip_existing: true,
            dry_run: false,
            folder_prefix: None,
            keep_local: None,
            keep_local_format: ExportFormat::Eml,
            retry_attempts: 3,
            backoff_base_secs: 3,
            backoff_max_secs: 60,
            verify: true,
            skip_all_mail: true,
        }
    }

    pub fn from_config(
        cfg: &AppConfig,
        source: AccountConfig,
        source_conn: ConnOptions,
        dest_conn: ConnOptions,
    ) -> Self {
        Self {
            source,
            source_conn,
            dest_conn,
            concurrency: cfg.restore_concurrency.max(1),
            skip_existing: cfg.dedup_on_restore,
            dry_run: false,
            folder_prefix: None,
            keep_local: None,
            keep_local_format: cfg.export_format,
            retry_attempts: cfg.retry_attempts,
            backoff_base_secs: cfg.retry_backoff_base_secs,
            backoff_max_secs: cfg.retry_backoff_max_secs,
            verify: cfg.verify_after_backup,
            skip_all_mail: cfg.skip_all_mail,
        }
    }
}

struct FolderPlan {
    source_name: String,
    target_name: String,
    special_use: Option<SpecialUse>,
    uid_validity: u32,
    exists: u64,
    uids: Vec<u32>,
}

/// Ejecuta la migración completa.
pub fn run(opts: &MigrateOptions, ctx: &Ctx) -> Result<RunReport> {
    let started = Instant::now();
    let label = format!("{} → {}", opts.source.email, opts.dest_conn.email);
    let mut report = RunReport::new(if opts.dry_run {
        "Migración (simulación)"
    } else {
        "Migración"
    });
    let mut account = AccountReport::new(
        format!("{} → {}", opts.source.email, opts.dest_conn.email),
        opts.source.host.clone(),
    );

    ctx.send(TaskEvent::Started {
        kind: TaskKind::Migrate,
        detail: label.clone(),
    });
    ctx.info(
        "migrate",
        format!(
            "=== MIGRACIÓN DIRECTA: {} ===\n  origen : {}\n  destino: {}{}",
            label,
            opts.source_conn.describe(),
            opts.dest_conn.describe(),
            if opts.dry_run { "\n  [SIMULACIÓN: no se escribe nada]" } else { "" }
        ),
    );

    // --- Fase 1: inventario del origen --------------------------------------
    let mut source_holder = SessionHolder::new(opts.source_conn.clone(), opts.source.email.clone());
    let remote_folders = connect::with_retry(
        ctx,
        &mut source_holder,
        "Listado de carpetas del origen",
        opts.retry_attempts,
        opts.backoff_base_secs,
        opts.backoff_max_secs,
        connect::list_folders,
    )?;

    let mut plans: Vec<FolderPlan> = Vec::new();
    for folder in &remote_folders {
        if !folder.selectable {
            continue;
        }
        if !folders::matches_filter(
            &folder.name,
            &opts.source.exclude_folders,
            opts.source.include_only_folders.as_deref(),
        ) {
            continue;
        }
        if opts.skip_all_mail && folder.special_use == Some(SpecialUse::All) {
            ctx.info(
                "migrate",
                format!("Carpeta '{}' omitida (buzón virtual \\All).", folder.name),
            );
            continue;
        }

        let name = folder.name.clone();
        let info = connect::with_retry(
            ctx,
            &mut source_holder,
            &format!("Examinar '{}'", name),
            opts.retry_attempts,
            opts.backoff_base_secs,
            opts.backoff_max_secs,
            |session| connect::select_mailbox(session, &name, true),
        )?;
        if info.exists == 0 {
            ctx.debug("migrate", format!("'{}' está vacía, se omite.", name));
            continue;
        }

        let metadata = connect::with_retry(
            ctx,
            &mut source_holder,
            &format!("Metadatos de '{}'", name),
            opts.retry_attempts,
            opts.backoff_base_secs,
            opts.backoff_max_secs,
            |session| connect::fetch_metadata(session, "1:*"),
        )?;

        plans.push(FolderPlan {
            source_name: folder.name.clone(),
            target_name: folder.name.clone(),
            special_use: folder.special_use,
            uid_validity: info.uid_validity,
            exists: info.exists,
            uids: metadata.iter().map(|message| message.uid).collect(),
        });
    }
    source_holder.disconnect();

    if plans.is_empty() {
        anyhow::bail!("No hay carpetas con mensajes para migrar.");
    }

    let total_messages: u64 = plans.iter().map(|plan| plan.uids.len() as u64).sum();
    let total_folders = plans.len();
    report.estimated_bytes = None;

    // --- Fase 2: descubrir el destino (delimitador y carpetas especiales) ----
    let mut dest_holder = SessionHolder::new(opts.dest_conn.clone(), opts.dest_conn.email.clone());
    let dest_folders = connect::with_retry(
        ctx,
        &mut dest_holder,
        "Listado de carpetas del destino",
        opts.retry_attempts,
        opts.backoff_base_secs,
        opts.backoff_max_secs,
        connect::list_folders,
    )?;
    let delimiter = dest_folders
        .iter()
        .find_map(|folder| folder.delimiter)
        .unwrap_or('/');
    let mut special_targets: HashMap<SpecialUse, String> = HashMap::new();
    for folder in &dest_folders {
        if let Some(special) = folder.special_use {
            special_targets.entry(special).or_insert_with(|| folder.name.clone());
        }
    }
    dest_holder.disconnect();

    for plan in &mut plans {
        let base = match &opts.folder_prefix {
            Some(prefix) if !prefix.trim().is_empty() => {
                let parts = folders::split_folder_path(&plan.source_name, None);
                format!(
                    "{}{}{}",
                    prefix.trim(),
                    delimiter,
                    parts.join(&delimiter.to_string())
                )
            }
            _ => plan.source_name.clone(),
        };
        plan.target_name = match plan.special_use {
            Some(special) => special_targets.get(&special).cloned().unwrap_or(base),
            None => base,
        };
    }

    ctx.info(
        "migrate",
        format!(
            "Inventario: {} mensaje(s) en {} carpeta(s) del origen.",
            total_messages, total_folders
        ),
    );

    if opts.dry_run {
        for plan in &plans {
            ctx.info(
                "migrate",
                format!(
                    "[simulación] '{}' → '{}': {} mensaje(s) (UIDVALIDITY {})",
                    plan.source_name,
                    plan.target_name,
                    plan.uids.len(),
                    plan.uid_validity
                ),
            );
            account.folders.push(FolderReport {
                folder: plan.source_name.clone(),
                remote_name: Some(plan.target_name.clone()),
                messages: plan.uids.len() as u64,
                server_count: Some(plan.exists),
                verified: true,
                ..Default::default()
            });
        }
        account.recompute();
        report.accounts.push(account);
        report.finish(started.elapsed().as_secs_f64(), false);
        ctx.send(TaskEvent::Finished {
            summary: Box::new(report.clone()),
        });
        return Ok(report);
    }

    // --- Fase 3: trabajo paralelo por rangos de UIDs ------------------------
    let work_items: Vec<(usize, usize, usize)> = {
        let mut items = Vec::new();
        for (position, plan) in plans.iter().enumerate() {
            let total = plan.uids.len();
            let mut start = 0usize;
            while start < total {
                let end = (start + UID_RANGE_SIZE).min(total);
                items.push((position, start, end));
                start = end;
            }
        }
        items
    };
    let work: Arc<Mutex<VecDeque<(usize, usize, usize)>>> =
        Arc::new(Mutex::new(work_items.into_iter().collect()));
    let folder_states: Arc<Mutex<HashMap<usize, FolderState>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let uploaded = Arc::new(AtomicU64::new(0));
    let skipped = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let local_copies: Option<Arc<LocalCopies>> = opts
        .keep_local
        .clone()
        .map(|base| Arc::new(LocalCopies::new(base, opts.keep_local_format)));

    let workers = opts.concurrency.min(work_items_count(&plans)).max(1);
    let plans_ref = &plans;
    let opts_ref = opts;
    let ctx_ref = ctx;

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let work = Arc::clone(&work);
            let folder_states = Arc::clone(&folder_states);
            let uploaded = Arc::clone(&uploaded);
            let skipped = Arc::clone(&skipped);
            let failed = Arc::clone(&failed);
            let bytes = Arc::clone(&bytes);
            let local_copies = local_copies.as_ref().map(Arc::clone);

            scope.spawn(move || {
                let mut source =
                    SessionHolder::new(opts_ref.source_conn.clone(), opts_ref.source.email.clone());
                let mut dest = SessionHolder::new(
                    opts_ref.dest_conn.clone(),
                    opts_ref.dest_conn.email.clone(),
                );
                let mut last_event = Instant::now();

                loop {
                    let next = { work.lock().expect("cola de trabajo").pop_front() };
                    let Some((position, start, end)) = next else { break };
                    if ctx_ref.cancelled() {
                        break;
                    }

                    let plan = &plans_ref[position];
                    let source_name = plan.source_name.clone();
                    let target = plan.target_name.clone();
                    let scope_label = format!("{} → {}", source_name, target);

                    // Preparar carpeta destino y deduplicación una sola vez por carpeta.
                    // El estado se reserva bajo lock y el trabajo de red se hace
                    // fuera del lock para no serializar a los trabajadores.
                    let needs_preparation = {
                        let mut states = folder_states.lock().expect("estados");
                        if let std::collections::hash_map::Entry::Vacant(e) = states.entry(position) {
                            e.insert(FolderState::default());
                            true
                        } else {
                            false
                        }
                    };
                    if needs_preparation {
                        let (created, existing_ids) =
                            prepare_folder(ctx_ref, &mut dest, opts_ref, &target);
                        let mut states = folder_states.lock().expect("estados");
                        if let Some(state) = states.get_mut(&position) {
                            state.created = created;
                            state.existing = existing_ids;
                            state.prepared = true;
                        }
                    }

                    let uid_list: Vec<u32> = plan.uids[start..end].to_vec();
                    let uid_set = crate::manifest::seq_set(&uid_list);

                    let fetched = connect::with_retry(
                        ctx_ref,
                        &mut source,
                        &format!("Descargar {} mensaje(s) de '{}'", uid_list.len(), source_name),
                        opts_ref.retry_attempts,
                        opts_ref.backoff_base_secs,
                        opts_ref.backoff_max_secs,
                        |session| {
                            connect::select_mailbox(session, &source_name, true)?;
                            connect::fetch_bodies(session, &uid_set)
                        },
                    );

                    let fetched = match fetched {
                        Ok(fetched) => fetched,
                        Err(err) => {
                            failed.fetch_add(uid_list.len() as u64, Ordering::Relaxed);
                            if crate::cancel::is_cancelled(&err) {
                                ctx_ref.warn(&scope_label, "Cancelado durante la descarga.");
                            } else {
                                ctx_ref.error(
                                    &scope_label,
                                    format!("Error leyendo del origen: {:#}", err),
                                );
                            }
                            continue;
                        }
                    };

                    for message in fetched {
                        if ctx_ref.cancelled() {
                            break;
                        }
                        let Some(body) = message.body.clone() else {
                            failed.fetch_add(1, Ordering::Relaxed);
                            continue;
                        };

                        let message_id = message
                            .message_id
                            .clone()
                            .or_else(|| crate::formats::header_value(&body, "message-id"));

                        if opts_ref.skip_existing {
                            if let Some(id) = message_id.as_deref() {
                                let normalized = connect::normalize_message_id(id);
                                let is_duplicate = {
                                    let states = folder_states.lock().expect("estados");
                                    states
                                        .get(&position)
                                        .map(|state| state.existing.contains(&normalized))
                                        .unwrap_or(false)
                                };
                                if is_duplicate {
                                    skipped.fetch_add(1, Ordering::Relaxed);
                                    let mut states = folder_states.lock().expect("estados");
                                    if let Some(state) = states.get_mut(&position) {
                                        state.skipped += 1;
                                    }
                                    continue;
                                }
                            }
                        }

                        let target_clone = target.clone();
                        let flags = message.flags.clone();
                        let internal_date = message.internal_date.clone();
                        let appended = connect::with_retry(
                            ctx_ref,
                            &mut dest,
                            &format!("APPEND en '{}'", target_clone),
                            opts_ref.retry_attempts,
                            opts_ref.backoff_base_secs,
                            opts_ref.backoff_max_secs,
                            |session| {
                                connect::append_message(
                                    session,
                                    &target_clone,
                                    &body,
                                    &flags,
                                    internal_date.as_deref(),
                                )
                            },
                        );

                        match appended {
                            Ok(()) => {
                                uploaded.fetch_add(1, Ordering::Relaxed);
                                bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                                let mut states = folder_states.lock().expect("estados");
                                if let Some(state) = states.get_mut(&position) {
                                    state.uploaded += 1;
                                    state.bytes += body.len() as u64;
                                }
                            }
                            Err(err) => {
                                failed.fetch_add(1, Ordering::Relaxed);
                                if !crate::cancel::is_cancelled(&err) {
                                    ctx_ref.error(
                                        &scope_label,
                                        format!("Error subiendo el UID {}: {:#}", message.uid, err),
                                    );
                                }
                                let mut states = folder_states.lock().expect("estados");
                                if let Some(state) = states.get_mut(&position) {
                                    state.errors += 1;
                                    state
                                        .errors_detail
                                        .push(format!("UID {}: {:#}", message.uid, err));
                                }
                                continue;
                            }
                        }

                        // Copia local opcional (serializada por carpeta: mbox es un
                        // archivo compartido y no admite escrituras concurrentes).
                        if let Some(local) = &local_copies {
                            if let Err(err) = local.write(
                                &source_name,
                                message.uid,
                                &body,
                                &flags,
                                internal_date.as_deref(),
                            ) {
                                ctx_ref.warn(
                                    &scope_label,
                                    format!("No se pudo escribir la copia local: {:#}", err),
                                );
                            }
                        }

                        let done =
                            uploaded.load(Ordering::Relaxed) + skipped.load(Ordering::Relaxed);
                        if last_event.elapsed().as_secs_f64() >= 0.25 || done >= total_messages {
                            ctx_ref.send(TaskEvent::Account {
                                account: opts_ref.dest_conn.email.clone(),
                                state: AccountRunState::Running {
                                    folder: scope_label.clone(),
                                    current: done,
                                    total: total_messages,
                                    bytes: bytes.load(Ordering::Relaxed),
                                },
                            });
                            last_event = Instant::now();
                        }
                    }
                }
                source.disconnect();
                dest.disconnect();
            });
        }
    });

    // --- Fase 4: reporte y verificación ------------------------------------
    let states = folder_states.lock().expect("estados").clone();
    for (position, plan) in plans.iter().enumerate() {
        let mut folder_report = FolderReport {
            folder: plan.source_name.clone(),
            remote_name: Some(plan.target_name.clone()),
            messages: plan.uids.len() as u64,
            server_count: Some(plan.exists),
            ..Default::default()
        };
        if let Some(state) = states.get(&position) {
            folder_report.uploaded = state.uploaded;
            folder_report.skipped = state.skipped;
            folder_report.errors = state.errors;
            folder_report.bytes = state.bytes;
            folder_report.created = state.created;
            folder_report.errors_detail = state.errors_detail.clone();
        }
        account.folders.push(folder_report);
    }

    if opts.verify && !ctx.cancelled() {
        let mut dest =
            SessionHolder::new(opts.dest_conn.clone(), opts.dest_conn.email.clone());
        for folder in account.folders.iter_mut() {
            let target = folder.remote_name.clone().unwrap_or_else(|| folder.folder.clone());
            let expected = folder.uploaded + folder.skipped;
            let actual = connect::with_retry(
                ctx,
                &mut dest,
                &format!("Verificar '{}'", target),
                opts.retry_attempts,
                opts.backoff_base_secs,
                opts.backoff_max_secs,
                |session| connect::select_mailbox(session, &target, true).map(|info| info.exists),
            );
            match actual {
                Ok(exists) => {
                    folder.server_count = Some(exists);
                    folder.local_count = expected;
                    folder.verified = exists >= expected;
                    if !folder.verified {
                        ctx.warn(
                            "migrate",
                            format!(
                                "'{}': el destino reporta {} mensaje(s) y se esperaban {}.",
                                target, exists, expected
                            ),
                        );
                    }
                }
                Err(err) => {
                    folder.verified = false;
                    ctx.warn("migrate", format!("No se pudo verificar '{}': {:#}", target, err));
                }
            }
        }
        dest.disconnect();
    }

    account.recompute();
    // El reporte de migración cuenta los subidos como "nuevos".
    account.new = account.uploaded;
    report.accounts.push(account);
    report.finish(started.elapsed().as_secs_f64(), ctx.cancelled());
    ctx.success("migrate", report.summary_line());
    ctx.send(TaskEvent::Finished {
        summary: Box::new(report.clone()),
    });
    Ok(report)
}

fn work_items_count(plans: &[FolderPlan]) -> usize {
    plans
        .iter()
        .map(|plan| plan.uids.len().div_ceil(UID_RANGE_SIZE).max(1))
        .sum()
}

#[derive(Debug, Clone, Default)]
struct FolderState {
    prepared: bool,
    existing: HashSet<String>,
    created: bool,
    uploaded: u64,
    skipped: u64,
    errors: usize,
    bytes: u64,
    errors_detail: Vec<String>,
}

/// Crea la carpeta destino (si hace falta) y carga los Message-ID existentes.
/// Devuelve `(carpeta_creada, message_ids_existentes)`.
fn prepare_folder(
    ctx: &Ctx,
    dest: &mut SessionHolder,
    opts: &MigrateOptions,
    target: &str,
) -> (bool, HashSet<String>) {
    let mut created = false;
    if !folders::is_inbox(target) {
        let name = target.to_string();
        if let Ok(session) = dest.session() {
            match session.create(&name) {
                Ok(()) => {
                    created = true;
                    ctx.success("migrate", format!("Carpeta '{}' creada en el destino.", name));
                }
                Err(_) => ctx.debug(
                    "migrate",
                    format!("'{}' ya existía o se creará implícitamente.", name),
                ),
            }
        }
    }

    if !opts.skip_existing {
        return (created, HashSet::new());
    }

    let name = target.to_string();
    match connect::with_retry(
        ctx,
        dest,
        &format!("Leer Message-ID existentes de '{}'", name),
        opts.retry_attempts,
        opts.backoff_base_secs,
        opts.backoff_max_secs,
        |session| connect::existing_message_ids(session, &name),
    ) {
        Ok(ids) => {
            if !ids.is_empty() {
                ctx.info(
                    "migrate",
                    format!(
                        "'{}': {} mensaje(s) ya presentes en el destino (se omitirán por Message-ID).",
                        name,
                        ids.len()
                    ),
                );
            }
            (created, ids)
        }
        Err(err) => {
            ctx.warn(
                "migrate",
                format!(
                    "No se pudieron leer los Message-ID de '{}': {:#}. Se sube sin deduplicar.",
                    name, err
                ),
            );
            (created, HashSet::new())
        }
    }
}

/// Copia local opcional (`--keep-local`): un escritor por carpeta de origen.
struct LocalCopies {
    base: PathBuf,
    format: ExportFormat,
    sinks: Mutex<HashMap<String, Arc<Mutex<FolderSink>>>>,
}

impl LocalCopies {
    fn new(base: PathBuf, format: ExportFormat) -> Self {
        Self {
            base,
            format,
            sinks: Mutex::new(HashMap::new()),
        }
    }

    /// Escribe la copia local de un mensaje migrado.
    ///
    /// Es una exportación simple (sin manifiesto): sirve como evidencia y para volver
    /// a importar desde la pestaña de restauración. La reanudación de una migración se
    /// apoya en el `Message-ID` del destino, no en esta copia.
    fn write(
        &self,
        source_folder: &str,
        uid: u32,
        body: &[u8],
        flags: &[String],
        internal_date: Option<&str>,
    ) -> Result<()> {
        let account_dir = self.base.clone();
        let format = self.format;
        let folder_dir = account_dir.join(folders::folder_relpath(source_folder, Some('/')));

        let sink = {
            let mut map = self.sinks.lock().expect("sinks");
            if !map.contains_key(source_folder) {
                let created = FolderSink::open(format, &account_dir, &folder_dir)?;
                map.insert(source_folder.to_string(), Arc::new(Mutex::new(created)));
            }
            Arc::clone(map.get(source_folder).expect("sink"))
        };

        let from_address = crate::formats::header_value(body, "From")
            .map(|value| crate::formats::address_of(&value))
            .unwrap_or_else(|| "unknown@localhost".to_string());
        let message_id = crate::formats::header_value(body, "message-id");

        let mut guard = sink.lock().expect("sink bloqueado");
        guard.store(
            &account_dir,
            source_folder,
            &folder_dir,
            uid,
            body,
            message_id.as_deref(),
            flags,
            internal_date,
            &from_address,
        )?;
        Ok(())
    }
}

/// Resumen del destino para mostrar en la UI antes de migrar.
pub fn describe_security(opts: &MigrateOptions) -> Vec<String> {
    let mut notes = Vec::new();
    if opts.source_conn.tls == TlsMode::Plain || opts.dest_conn.tls == TlsMode::Plain {
        notes.push("Hay extremos sin cifrado: las credenciales viajan en claro.".to_string());
    }
    if opts.skip_existing {
        notes.push("Se omitirán los mensajes ya presentes en el destino (por Message-ID).".to_string());
    } else {
        notes.push(
            "La deduplicación está desactivada: volver a migrar duplicará los mensajes.".to_string(),
        );
    }
    notes
}

/// Utilidad para la UI: crea un `ConnOptions` de destino desde datos del formulario.
pub fn dest_conn_options(
    host: &str,
    port: u16,
    tls: TlsMode,
    email: &str,
    password: &str,
    timeout_secs: u64,
) -> ConnOptions {
    ConnOptions {
        host: host.trim().to_string(),
        port,
        tls,
        email: email.trim().to_string(),
        password: Some(crate::config::clean_secret(password)),
        oauth_token: None,
        timeout: std::time::Duration::from_secs(timeout_secs.max(1)),
        insecure_skip_tls_verify: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_items_split_large_folders_into_ranges() {
        let plans = vec![
            FolderPlan {
                source_name: "INBOX".into(),
                target_name: "INBOX".into(),
                special_use: None,
                uid_validity: 1,
                exists: 1200,
                uids: (1..=1200).collect(),
            },
            FolderPlan {
                source_name: "Vacia".into(),
                target_name: "Vacia".into(),
                special_use: None,
                uid_validity: 1,
                exists: 0,
                uids: Vec::new(),
            },
        ];
        assert_eq!(work_items_count(&plans), 4); // 3 rangos de 500 + la vacía
    }

    #[test]
    fn dest_options_clean_secrets() {
        let conn = dest_conn_options(" imap.host.com ", 993, TlsMode::Implicit, " a@b.com ", "pwd\r\n", 45);
        assert_eq!(conn.host, "imap.host.com");
        assert_eq!(conn.email, "a@b.com");
        assert_eq!(conn.password.as_deref(), Some("pwd"));
    }

    #[test]
    fn security_notes_mention_plaintext_and_dedup() {
        let mut opts = MigrateOptions::new(
            AccountConfig::new("a@b.com", "h"),
            dest_conn_options("h", 143, TlsMode::Plain, "a@b.com", "x", 30),
            dest_conn_options("h2", 993, TlsMode::Implicit, "c@d.com", "x", 30),
        );
        opts.skip_existing = false;
        let notes = describe_security(&opts);
        assert!(notes.iter().any(|n| n.contains("sin cifrado")));
        assert!(notes.iter().any(|n| n.contains("duplicará")));
    }
}
