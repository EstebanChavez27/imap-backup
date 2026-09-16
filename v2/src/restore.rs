// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Motor de restauración / importación masiva.
//!
//! Diferencias clave frente a la v1:
//!
//! * **Streaming**: el índice guarda solo rutas/desplazamientos, no los bytes. Un ZIP
//!   o un mbox de varios GB se restaura con memoria acotada, y el escaneo se hace en
//!   el hilo de trabajo (no congela la interfaz).
//! * **Idempotente**: antes de subir se leen los `Message-ID` ya presentes en el
//!   destino; volver a ejecutar la restauración no duplica correos.
//! * **Fiel**: se envían los flags y la fecha interna reales (`APPEND ... FLAGS ...
//!   INTERNALDATE`), tomados del manifiesto o de los flags de Maildir.
//! * **Paralelo**: las carpetas se preparan y los mensajes se suben con
//!   `restore_concurrency` trabajadores, cada uno con su propia sesión IMAP.
//! * **Carpetas especiales**: si el origen trae "Enviados" y el destino expone `\Sent`,
//!   el contenido va a la carpeta correcta del destino.

use crate::archive::ZipSource;
use crate::config::AppConfig;
use crate::connect::{self, ConnOptions, SessionHolder};
use crate::events::{AccountRunState, Ctx, TaskEvent, TaskKind};
use crate::folders::{self, SpecialUse};
use crate::formats::maildir;
use crate::formats::mbox::{self, MboxReader};
use crate::manifest::{AccountManifest, MessageEntry};
use crate::report::{AccountReport, FolderReport, RunReport};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const UPLOAD_CHUNK_ITEMS: usize = 200;

#[derive(Debug, Clone)]
pub struct RestoreOptions {
    pub source: PathBuf,
    pub dest: ConnOptions,
    pub concurrency: usize,
    pub skip_existing: bool,
    pub dry_run: bool,
    /// Prefijo opcional para no mezclar con lo existente (p. ej. `Migrado`).
    pub folder_prefix: Option<String>,
    pub retry_attempts: u32,
    pub backoff_base_secs: u64,
    pub backoff_max_secs: u64,
    pub verify: bool,
}

impl RestoreOptions {
    pub fn from_config(cfg: &AppConfig, source: PathBuf, dest: ConnOptions) -> Self {
        Self {
            source,
            dest,
            concurrency: cfg.restore_concurrency.max(1),
            skip_existing: cfg.dedup_on_restore,
            dry_run: false,
            folder_prefix: None,
            retry_attempts: cfg.retry_attempts,
            backoff_base_secs: cfg.retry_backoff_base_secs,
            backoff_max_secs: cfg.retry_backoff_max_secs,
            verify: cfg.verify_after_backup,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// Directorio con archivos `.eml`.
    EmlDir,
    /// Directorio (o archivo) con archivos mbox.
    Mbox,
    /// Directorio con estructura Maildir.
    Maildir,
    /// Archivo ZIP.
    Zip,
    /// Árbol mixto: se detectó más de un formato.
    Mixed,
}

impl SourceKind {
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::EmlDir => "carpeta de .eml",
            SourceKind::Mbox => "mbox",
            SourceKind::Maildir => "Maildir",
            SourceKind::Zip => "ZIP",
            SourceKind::Mixed => "árbol mixto (.eml/.mbox/Maildir)",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Locator {
    /// Índice dentro de un archivo ZIP.
    Zip(usize),
    /// Mensaje dentro de un mbox guardado como entrada de un ZIP.
    ZipMbox {
        index: usize,
        start: u64,
        end: u64,
    },
    /// Archivo suelto en disco.
    File(PathBuf),
    /// Mensaje `index` dentro de un archivo mbox.
    Mbox { path: PathBuf, index: usize },
}

#[derive(Debug, Clone)]
pub struct ItemRef {
    pub name: String,
    pub size: u64,
    pub locator: Locator,
    pub message_id: Option<String>,
    pub flags: Vec<String>,
    pub internal_date: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FolderSource {
    /// Nombre remoto de destino decidido al construir el índice.
    pub remote_name: String,
    pub special_use: Option<SpecialUse>,
    pub items: Vec<ItemRef>,
}

impl FolderSource {
    pub fn total_bytes(&self) -> u64 {
        self.items.iter().map(|item| item.size).sum()
    }
}

/// Índice del origen: quién es quién y dónde está, sin cargar los mensajes.
#[derive(Debug, Clone)]
pub struct SourceIndex {
    pub kind: SourceKind,
    pub folders: Vec<FolderSource>,
    pub manifest: Option<AccountManifest>,
    pub total_messages: usize,
    pub total_bytes: u64,
    pub folders_detected: usize,
}

impl SourceIndex {
    pub fn summary(&self) -> String {
        format!(
            "{} mensaje(s) en {} carpeta(s) ({}) desde un origen {}",
            self.total_messages,
            self.folders_detected,
            crate::fsutil::human_bytes(self.total_bytes),
            self.kind.label()
        )
    }
}

/// Lector por trabajador: abre su propio ZIP y mantiene lectores mbox con `seek`.
struct SourceReader {
    zip: Option<ZipSource>,
    mboxes: HashMap<PathBuf, MboxReader>,
}

impl SourceReader {
    fn open(source: &Path) -> Result<Self> {
        let zip = if source.is_file()
            && source
                .extension()
                .map(|e| e.eq_ignore_ascii_case("zip"))
                .unwrap_or(false)
        {
            Some(ZipSource::open(source)?)
        } else {
            None
        };
        Ok(Self {
            zip,
            mboxes: HashMap::new(),
        })
    }

    fn read(&mut self, locator: &Locator) -> Result<Vec<u8>> {
        match locator {
            Locator::Zip(index) => {
                let zip = self
                    .zip
                    .as_mut()
                    .context("El archivo ZIP no está abierto")?;
                zip.read_entry(*index)
            }
            Locator::ZipMbox { index, start, end } => {
                let zip = self
                    .zip
                    .as_mut()
                    .context("El archivo ZIP no está abierto")?;
                let stored = zip.read_entry_range(*index, *start, end.saturating_sub(*start))?;
                Ok(crate::formats::mbox::decode_stored_message(&stored))
            }
            Locator::File(path) => {
                std::fs::read(path).with_context(|| format!("No se pudo leer {}", path.display()))
            }
            Locator::Mbox { path, index } => {
                if !self.mboxes.contains_key(path) {
                    let reader = MboxReader::open(path)?;
                    self.mboxes.insert(path.clone(), reader);
                }
                let reader = self.mboxes.get_mut(path).expect("lector mbox");
                reader.message(*index)
            }
        }
    }
}

/// Construye el índice del origen (sin cargar mensajes en memoria).
pub fn build_index(ctx: &Ctx, source: &Path) -> Result<SourceIndex> {
    if !source.exists() {
        anyhow::bail!("La ruta de origen no existe: {}", source.display());
    }

    let manifest = find_manifest(source);
    let entries_by_id = manifest
        .as_ref()
        .map(index_manifest_entries)
        .unwrap_or_default();

    ctx.send(TaskEvent::Started {
        kind: TaskKind::Scan,
        detail: format!("Analizando {}", source.display()),
    });
    ctx.info(
        "restore",
        format!("Analizando origen: {}", source.display()),
    );

    let mut folders: HashMap<String, FolderSource> = HashMap::new();
    let mut detected: HashSet<SourceKind> = HashSet::new();
    let mut total_messages = 0usize;
    let mut total_bytes = 0u64;
    let mut scanned_files = 0u64;

    let push_item = |folder_name: String,
                         special_use: Option<SpecialUse>,
                         item: ItemRef,
                         detected_kind: SourceKind,
                         folders: &mut HashMap<String, FolderSource>,
                         detected: &mut HashSet<SourceKind>,
                         total_messages: &mut usize,
                         total_bytes: &mut u64| {
        *total_messages += 1;
        *total_bytes = total_bytes.saturating_add(item.size);
        detected.insert(detected_kind);
        let entry = folders
            .entry(folder_name.clone())
            .or_insert_with(|| FolderSource {
                remote_name: folder_name,
                special_use,
                items: Vec::new(),
            });
        if entry.special_use.is_none() {
            entry.special_use = special_use;
        }
        entry.items.push(item);
    };

    if source.is_file() {
        let extension = source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if extension == "zip" {
            let mut zip = ZipSource::open(source)?;
            let entries = zip.entries().to_vec();
            for entry in entries {
                if entry.is_dir || is_ignored_file(&entry.name) {
                    continue;
                }
                let lower = entry.name.to_lowercase();
                let is_mbox = lower.ends_with(".mbox");
                if !is_mbox && !lower.ends_with(".eml") && !lower.contains("/cur/") && !lower.contains("/new/") {
                    // Dentro de un ZIP se restauran mensajes sueltos, Maildir y mbox.
                    ctx.debug(
                        "restore",
                        format!("Entrada ignorada en el ZIP: {}", entry.name),
                    );
                    continue;
                }
                let remote = folders::remote_name_from_relpath(
                    Path::new(&entry.name),
                    manifest_remote_for(&manifest, &entry.name),
                );
                if is_mbox {
                    // Un mbox puede contener cientos de mensajes: se indexa y cada uno
                    // se lee bajo demanda, sin descomprimir el archivo completo.
                    let folder_special = special_use_for(&manifest, &remote);
                    let offsets = zip.scan_entry_mbox(entry.index)?;
                    for (position, (start, end)) in offsets.iter().enumerate() {
                        push_item(
                            remote.clone(),
                            folder_special,
                            ItemRef {
                                name: format!("{}#{}", remote, position),
                                size: end.saturating_sub(*start),
                                locator: Locator::ZipMbox {
                                    index: entry.index,
                                    start: *start,
                                    end: *end,
                                },
                                message_id: None,
                                flags: Vec::new(),
                                internal_date: None,
                            },
                            SourceKind::Zip,
                            &mut folders,
                            &mut detected,
                            &mut total_messages,
                            &mut total_bytes,
                        );
                    }
                    continue;
                }
                let folder_special = special_use_for(&manifest, &remote);
                push_item(
                    remote.clone(),
                    folder_special,
                    ItemRef {
                        name: entry.name.clone(),
                        size: entry.size,
                        locator: Locator::Zip(entry.index),
                        message_id: None,
                        flags: Vec::new(),
                        internal_date: None,
                    },
                    SourceKind::Zip,
                    &mut folders,
                    &mut detected,
                    &mut total_messages,
                    &mut total_bytes,
                );
            }
        } else if extension == "mbox" {
            let offsets = mbox::scan_offsets(source)?;
            let remote = folders::remote_name_from_relpath(source, None);
            for (index, (start, end)) in offsets.iter().enumerate() {
                let (start, end) = (*start, *end);
                push_item(
                    remote.clone(),
                    None,
                    ItemRef {
                        name: format!("{}#{}", remote, index),
                        size: end.saturating_sub(start),
                        locator: Locator::Mbox {
                            path: source.to_path_buf(),
                            index,
                        },
                        message_id: None,
                        flags: Vec::new(),
                        internal_date: None,
                    },
                    SourceKind::Mbox,
                    &mut folders,
                    &mut detected,
                    &mut total_messages,
                    &mut total_bytes,
                );
            }
        } else {
            anyhow::bail!(
                "El archivo de origen debe ser .zip o .mbox (recibido: {})",
                source.display()
            );
        }
    } else {
        for entry in walkdir::WalkDir::new(source)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if ctx.cancelled() {
                break;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let relative = match path.strip_prefix(source) {
                Ok(relative) => relative,
                Err(_) => continue,
            };
            let name = relative.to_string_lossy().replace('\\', "/");
            if is_ignored_file(&name) {
                continue;
            }
            scanned_files += 1;
            if scanned_files.is_multiple_of(200) {
                ctx.send(TaskEvent::ScanProgress {
                    detail: source.display().to_string(),
                    messages: total_messages as u64,
                    folders: folders.len() as u64,
                    bytes: total_bytes,
                });
            }

            let is_maildir = name.contains("/cur/")
                || name.contains("/new/")
                || path
                    .parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n == "cur" || n == "new")
                    .unwrap_or(false);

            let lower = name.to_lowercase();
            if lower.ends_with(".eml") {
                let remote = folders::remote_name_from_relpath(relative, None);
                let key = manifest_key_for(&manifest, &remote);
                let (flags, internal_date, message_id) = manifest_metadata(&entries_by_id, &key, None);
                push_item(
                    key.clone(),
                    special_use_for(&manifest, &key),
                    ItemRef {
                        name,
                        size: crate::fsutil::file_len(path).unwrap_or(0),
                        locator: Locator::File(path.to_path_buf()),
                        message_id,
                        flags,
                        internal_date,
                    },
                    SourceKind::EmlDir,
                    &mut folders,
                    &mut detected,
                    &mut total_messages,
                    &mut total_bytes,
                );
            } else if lower.ends_with(".mbox") {
                let offsets = mbox::scan_offsets(path)?;
                let remote = folders::remote_name_from_relpath(relative, None);
                let key = manifest_key_for(&manifest, &remote);
                for (index, (start, end)) in offsets.iter().enumerate() {
                    let (start, end) = (*start, *end);
                    push_item(
                        key.clone(),
                        special_use_for(&manifest, &key),
                        ItemRef {
                            name: format!("{}#{}", name, index),
                            size: end.saturating_sub(start),
                            locator: Locator::Mbox {
                                path: path.to_path_buf(),
                                index,
                            },
                            message_id: None,
                            flags: Vec::new(),
                            internal_date: None,
                        },
                        SourceKind::Mbox,
                        &mut folders,
                        &mut detected,
                        &mut total_messages,
                        &mut total_bytes,
                    );
                }
            } else if is_maildir {
                // La carpeta remota es el directorio que contiene cur/new.
                let maildir_root = match path.parent() {
                    Some(dir) if dir.file_name().map(|n| n == "cur" || n == "new").unwrap_or(false) => {
                        dir.parent().unwrap_or(dir)
                    }
                    Some(dir) => dir,
                    None => source,
                };
                let relative_dir = match maildir_root.strip_prefix(source) {
                    Ok(rel) => rel.to_path_buf(),
                    Err(_) => PathBuf::new(),
                };
                let remote = folders::remote_name_from_relpath(&relative_dir, None);
                let key = manifest_key_for(&manifest, &remote);
                let mut flags = maildir::flags_from_path(path);
                let (manifest_flags, internal_date, message_id) =
                    manifest_metadata(&entries_by_id, &key, None);
                if flags.is_empty() {
                    flags = manifest_flags;
                }
                push_item(
                    key.clone(),
                    special_use_for(&manifest, &key),
                    ItemRef {
                        name,
                        size: crate::fsutil::file_len(path).unwrap_or(0),
                        locator: Locator::File(path.to_path_buf()),
                        message_id,
                        flags,
                        internal_date,
                    },
                    SourceKind::Maildir,
                    &mut folders,
                    &mut detected,
                    &mut total_messages,
                    &mut total_bytes,
                );
            }
        }
    }

    if folders.is_empty() {
        anyhow::bail!(
            "No se encontraron mensajes (.eml, .mbox o Maildir) en {}",
            source.display()
        );
    }

    let mut folders: Vec<FolderSource> = folders.into_values().collect();
    folders.sort_by(|a, b| a.remote_name.cmp(&b.remote_name));

    let kind = match detected.len() {
        0 => SourceKind::Mixed,
        1 => *detected.iter().next().expect("un formato"),
        _ => SourceKind::Mixed,
    };

    let index = SourceIndex {
        kind,
        folders_detected: folders.len(),
        total_messages,
        total_bytes,
        folders,
        manifest,
    };
    ctx.success("restore", format!("Origen analizado: {}", index.summary()));
    Ok(index)
}

/// Ejecuta la restauración completa.
pub fn run(index: SourceIndex, opts: &RestoreOptions, ctx: &Ctx) -> Result<RunReport> {
    let started = Instant::now();
    let mut report = RunReport::new(if opts.dry_run {
        "Restauración (simulación)"
    } else {
        "Restauración"
    });
    report.estimated_bytes = Some(index.total_bytes);

    // Índice del manifiesto (si el origen trae uno) para recuperar flags y fechas
    // reales incluso en orígenes donde el formato no los guarda junto al mensaje.
    let manifest_entries: ManifestIndex = index
        .manifest
        .as_ref()
        .map(index_manifest_entries)
        .unwrap_or_default();

    let mut account = AccountReport::new(opts.dest.email.clone(), opts.dest.host.clone());

    ctx.send(TaskEvent::Started {
        kind: TaskKind::Restore,
        detail: format!("{} → {}", index.summary(), opts.dest.describe()),
    });

    if opts.dry_run {
        for folder in &index.folders {
            ctx.info(
                "restore",
                format!(
                    "[simulación] '{}': se subirían {} mensaje(s) ({})",
                    folder.remote_name,
                    folder.items.len(),
                    crate::fsutil::human_bytes(folder.total_bytes())
                ),
            );
            account.folders.push(FolderReport {
                folder: folder.remote_name.clone(),
                remote_name: Some(folder.remote_name.clone()),
                messages: folder.items.len() as u64,
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

    // --- Fase 1: descubrir el destino y preparar carpetas -------------------
    let mut bootstrap = SessionHolder::new(opts.dest.clone(), opts.dest.email.clone());
    let remote_folders = connect::with_retry(
        ctx,
        &mut bootstrap,
        "Listado de carpetas del destino",
        opts.retry_attempts,
        opts.backoff_base_secs,
        opts.backoff_max_secs,
        connect::list_folders,
    )?;

    let delimiter = remote_folders
        .iter()
        .find_map(|folder| folder.delimiter)
        .unwrap_or('/');
    let existing: HashSet<String> = remote_folders
        .iter()
        .map(|folder| folder.name.to_lowercase())
        .collect();

    // Mapeo de carpetas especiales del destino (para no duplicar "Enviados").
    let mut special_targets: HashMap<SpecialUse, String> = HashMap::new();
    for folder in &remote_folders {
        if let Some(special) = folder.special_use {
            special_targets.entry(special).or_insert_with(|| folder.name.clone());
        }
    }

    let plans: Vec<(String, Option<SpecialUse>)> = index
        .folders
        .iter()
        .map(|folder| {
            let base = match &opts.folder_prefix {
                Some(prefix) if !prefix.trim().is_empty() => {
                    let parts = folders::split_folder_path(&folder.remote_name, None);
                    format!("{}{}{}", prefix.trim(), delimiter, parts.join(&delimiter.to_string()))
                }
                _ => folder.remote_name.clone(),
            };
            (base, folder.special_use)
        })
        .collect();

    let mut targets: Vec<String> = Vec::with_capacity(plans.len());
    for (base, special) in &plans {
        let target = match special {
            Some(special_use) => special_targets
                .get(special_use)
                .cloned()
                .unwrap_or_else(|| base.clone()),
            None => base.clone(),
        };
        targets.push(target);
    }

    bootstrap.disconnect();

    // --- Fase 1b: crear carpetas y leer Message-ID existentes ---------------
    let work: Arc<Mutex<VecDeque<usize>>> =
        Arc::new(Mutex::new((0..index.folders.len()).collect()));
    let prepared: Arc<Mutex<HashMap<usize, HashSet<String>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let created: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(existing.clone()));
    let workers = opts.concurrency.min(index.folders.len()).max(1);

    let folder_ref = &index.folders;
    let targets_ref = &targets;
    let manifest_ref = &manifest_entries;
    let opts_ref = opts;
    let ctx_ref = ctx;

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let work = Arc::clone(&work);
            let prepared = Arc::clone(&prepared);
            let created = Arc::clone(&created);
            scope.spawn(move || {
                let mut holder = SessionHolder::new(opts_ref.dest.clone(), opts_ref.dest.email.clone());
                loop {
                    let next = { work.lock().expect("cola de carpetas").pop_front() };
                    let Some(index_position) = next else { break };
                    if ctx_ref.cancelled() {
                        break;
                    }

                    let folder = &folder_ref[index_position];
                    let target = &targets_ref[index_position];
                    let scope = format!("{} → {}", folder.remote_name, target);
                    let result = (|| -> Result<()> {
                        let already_known = {
                            created.lock().expect("creadas").contains(&target.to_lowercase())
                        };
                        // Crear sin reintentos: si la carpeta ya existe el servidor
                        // responde NO y reintentar solo perdería tiempo.
                        if !folders::is_inbox(target) && !already_known {
                            let name = target.clone();
                            let session = holder.session()?;
                            match session.create(&name) {
                                Ok(()) => {
                                    ctx_ref.success(
                                        &scope,
                                        format!("Carpeta '{}' creada en el destino.", target),
                                    );
                                    created
                                        .lock()
                                        .expect("creadas")
                                        .insert(target.to_lowercase());
                                }
                                Err(err) => ctx_ref.warn(
                                    &scope,
                                    format!(
                                        "No se pudo crear '{}' ({}). Se intentará subir igual.",
                                        target, err
                                    ),
                                ),
                            }
                        }

                        let ids = if opts_ref.skip_existing {
                            let name = target.clone();
                            connect::with_retry(
                                ctx_ref,
                                &mut holder,
                                &format!("Leer Message-ID existentes de '{}'", name),
                                opts_ref.retry_attempts,
                                opts_ref.backoff_base_secs,
                                opts_ref.backoff_max_secs,
                                |session| connect::existing_message_ids(session, &name),
                            )
                            .unwrap_or_default()
                        } else {
                            HashSet::new()
                        };
                        prepared.lock().expect("preparadas").insert(index_position, ids);
                        Ok(())
                    })();

                    if let Err(err) = result {
                        if crate::cancel::is_cancelled(&err) {
                            ctx_ref.warn(&scope, "Cancelado durante la preparación.");
                        } else {
                            ctx_ref.error(&scope, format!("Error preparando la carpeta: {:#}", err));
                        }
                        prepared
                            .lock()
                            .expect("preparadas")
                            .entry(index_position)
                            .or_default();
                    }
                }
                holder.disconnect();
            });
        }
    });

    // --- Fase 2: subir los mensajes en paralelo -----------------------------
    let chunks: Vec<(usize, usize, usize)> = index
        .folders
        .iter()
        .enumerate()
        .flat_map(|(position, folder)| {
            let total = folder.items.len();
            let mut out = Vec::new();
            let mut start = 0usize;
            while start < total {
                let end = (start + UPLOAD_CHUNK_ITEMS).min(total);
                out.push((position, start, end));
                start = end;
            }
            out
        })
        .collect();

    let total_items: u64 = index.total_messages as u64;
    let work: Arc<Mutex<VecDeque<(usize, usize, usize)>>> =
        Arc::new(Mutex::new(chunks.into_iter().collect()));
    let uploaded = Arc::new(AtomicU64::new(0));
    let skipped = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let per_folder: Arc<Mutex<HashMap<usize, FolderReport>>> =
        Arc::new(Mutex::new(HashMap::new()));

    for (position, folder) in index.folders.iter().enumerate() {
        per_folder.lock().expect("reporte").insert(
            position,
            FolderReport {
                folder: folder.remote_name.clone(),
                remote_name: Some(targets[position].clone()),
                messages: folder.items.len() as u64,
                ..Default::default()
            },
        );
    }

    let source_path = opts.source.clone();

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let work = Arc::clone(&work);
            let prepared = Arc::clone(&prepared);
            let per_folder = Arc::clone(&per_folder);
            let uploaded = Arc::clone(&uploaded);
            let skipped = Arc::clone(&skipped);
            let failed = Arc::clone(&failed);
            let bytes = Arc::clone(&bytes);

            let worker_source = source_path.clone();
            scope.spawn(move || {
                let mut holder = SessionHolder::new(opts_ref.dest.clone(), opts_ref.dest.email.clone());
                let mut reader = match SourceReader::open(&worker_source) {
                    Ok(reader) => reader,
                    Err(err) => {
                        ctx_ref.error("restore", format!("No se pudo abrir el origen: {:#}", err));
                        return;
                    }
                };
                let mut last_event = Instant::now();

                loop {
                    let next = { work.lock().expect("cola de trabajo").pop_front() };
                    let Some((position, start, end)) = next else { break };
                    if ctx_ref.cancelled() {
                        break;
                    }

                    let folder = &folder_ref[position];
                    let target = targets_ref[position].clone();
                    let scope_label = target.clone();
                    let existing_ids = {
                        prepared
                            .lock()
                            .expect("preparadas")
                            .get(&position)
                            .cloned()
                            .unwrap_or_default()
                    };

                    for item_index in start..end {
                        if ctx_ref.cancelled() {
                            break;
                        }
                        let item = &folder.items[item_index];

                        let body = match reader.read(&item.locator) {
                            Ok(body) => body,
                            Err(err) => {
                                failed.fetch_add(1, Ordering::Relaxed);
                                ctx_ref.error(
                                    &scope_label,
                                    format!("No se pudo leer '{}': {:#}", item.name, err),
                                );
                                continue;
                            }
                        };

                        let message_id = item
                            .message_id
                            .clone()
                            .or_else(|| crate::formats::header_value(&body, "message-id"))
                            .or_else(|| maildir::message_id_of(&body));

                        if opts_ref.skip_existing {
                            if let Some(id) = &message_id {
                                let normalized = connect::normalize_message_id(id);
                                if !normalized.is_empty() && existing_ids.contains(&normalized) {
                                    skipped.fetch_add(1, Ordering::Relaxed);
                                    if let Some(entry) = per_folder.lock().expect("reporte").get_mut(&position) {
                                        entry.skipped += 1;
                                    }
                                    continue;
                                }
                            }
                        }

                        // Flags y fecha: primero lo que trae el origen (Maildir, .eml
                        // junto al manifiesto) y, si falta, el manifiesto por Message-ID.
                        let mut flags = item.flags.clone();
                        let mut internal_date = item.internal_date.clone();
                        if flags.is_empty() && internal_date.is_none() {
                            if let Some(id) = message_id.as_deref() {
                                if let Some((folder_key, entry)) =
                                    manifest_ref.get(&connect::normalize_message_id(id))
                                {
                                    if folder_key.eq_ignore_ascii_case(&folder.remote_name) {
                                        flags = entry.flags.clone();
                                        internal_date = entry.internal_date.clone();
                                    }
                                }
                            }
                        }
                        let result = connect::with_retry(
                            ctx_ref,
                            &mut holder,
                            &format!("APPEND de '{}'", item.name),
                            opts_ref.retry_attempts,
                            opts_ref.backoff_base_secs,
                            opts_ref.backoff_max_secs,
                            |session| {
                                connect::append_message(
                                    session,
                                    &target,
                                    &body,
                                    &flags,
                                    internal_date.as_deref(),
                                )
                            },
                        );

                        match result {
                            Ok(()) => {
                                uploaded.fetch_add(1, Ordering::Relaxed);
                                bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                                if let Some(entry) = per_folder.lock().expect("reporte").get_mut(&position) {
                                    entry.uploaded += 1;
                                    entry.bytes += body.len() as u64;
                                }
                            }
                            Err(err) => {
                                failed.fetch_add(1, Ordering::Relaxed);
                                if crate::cancel::is_cancelled(&err) {
                                    ctx_ref.warn(&scope_label, "Cancelado durante la subida.");
                                } else {
                                    ctx_ref.error(
                                        &scope_label,
                                        format!("Error subiendo '{}': {:#}", item.name, err),
                                    );
                                }
                                if let Some(entry) = per_folder.lock().expect("reporte").get_mut(&position) {
                                    entry.errors += 1;
                                    entry.errors_detail.push(format!("{}: {:#}", item.name, err));
                                }
                            }
                        }

                        let done = uploaded.load(Ordering::Relaxed) + skipped.load(Ordering::Relaxed);
                        if last_event.elapsed().as_secs_f64() >= 0.25 || done >= total_items {
                            ctx_ref.send(TaskEvent::Account {
                                account: opts_ref.dest.email.clone(),
                                state: AccountRunState::Running {
                                    folder: scope_label.clone(),
                                    current: done,
                                    total: total_items,
                                    bytes: bytes.load(Ordering::Relaxed),
                                },
                            });
                            last_event = Instant::now();
                        }
                    }
                }
                holder.disconnect();
            });
        }
    });

    let reports: Vec<FolderReport> = {
        let map = per_folder.lock().expect("reporte");
        let mut list: Vec<(usize, FolderReport)> = map.iter().map(|(k, v)| (*k, v.clone())).collect();
        list.sort_by_key(|(position, _)| *position);
        list.into_iter().map(|(_, report)| report).collect()
    };
    account.folders = reports;
    account.recompute();

    // --- Fase 3: verificación ----------------------------------------------
    if opts.verify && !ctx.cancelled() {
        let mut holder = SessionHolder::new(opts.dest.clone(), opts.dest.email.clone());
        for (position, folder) in account.folders.iter_mut().enumerate() {
            let target = match targets.get(position) {
                Some(target) => target.clone(),
                None => continue,
            };
            let expected = folder.local_count.max(folder.uploaded) + folder.skipped;
            let actual = connect::with_retry(
                ctx,
                &mut holder,
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
                            "restore",
                            format!(
                                "'{}': el destino reporta {} mensaje(s) y se esperaban {}.",
                                target, exists, expected
                            ),
                        );
                    }
                }
                Err(err) => {
                    folder.verified = false;
                    ctx.warn(
                        "restore",
                        format!("No se pudo verificar '{}': {:#}", target, err),
                    );
                }
            }
        }
        holder.disconnect();
    }

    account.recompute();
    report.accounts.push(account);
    report.finish(started.elapsed().as_secs_f64(), ctx.cancelled());

    ctx.success("restore", report.summary_line());
    ctx.send(TaskEvent::Finished {
        summary: Box::new(report.clone()),
    });
    Ok(report)
}

// ---------------------------------------------------------------------------
// Utilidades del origen
// ---------------------------------------------------------------------------

fn is_ignored_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".json")
        || lower.ends_with(".txt")
        || lower.ends_with(".md")
        || lower.ends_with(".log")
        || lower.ends_with(".csv")
        || lower.ends_with(".part")
        || lower.ends_with(".tmp")
        || lower.ends_with(".eml.part")
        || lower.contains("/tmp/")
}

/// Busca el manifiesto en la ruta indicada, su padre o un nivel por debajo.
fn find_manifest(source: &Path) -> Option<AccountManifest> {
    if source.is_file() {
        let parent = source.parent()?;
        return AccountManifest::load(parent).or_else(|| {
            AccountManifest::load(parent.parent().unwrap_or(parent))
        });
    }
    if let Some(manifest) = AccountManifest::load(source) {
        return Some(manifest);
    }
    if let Some(parent) = source.parent() {
        if let Some(manifest) = AccountManifest::load(parent) {
            return Some(manifest);
        }
    }
    // Un nivel por debajo (el usuario apuntó al directorio de un dominio con
    // varias cuentas).
    if let Ok(entries) = std::fs::read_dir(source) {
        for entry in entries.filter_map(|e| e.ok()) {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let candidate = entry.path();
                if let Some(manifest) = AccountManifest::load(&candidate) {
                    return Some(manifest);
                }
                if let Ok(sub_entries) = std::fs::read_dir(&candidate) {
                    for sub in sub_entries.filter_map(|e| e.ok()) {
                        if sub.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                            if let Some(manifest) = AccountManifest::load(&sub.path()) {
                                return Some(manifest);
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

type ManifestIndex = HashMap<String, (String, MessageEntry)>;

fn index_manifest_entries(manifest: &AccountManifest) -> ManifestIndex {
    let mut map: ManifestIndex = HashMap::new();
    for (folder_name, folder) in &manifest.folders {
        for entry in &folder.messages {
            if let Some(id) = &entry.message_id {
                map.insert(
                    connect::normalize_message_id(id),
                    (folder_name.clone(), entry.clone()),
                );
            }
        }
    }
    map
}

fn manifest_remote_for<'a>(
    manifest: &'a Option<AccountManifest>,
    path_name: &str,
) -> Option<&'a str> {
    let manifest = manifest.as_ref()?;
    let candidate = folders::remote_name_from_relpath(Path::new(path_name), None);
    manifest
        .folders
        .keys()
        .find(|name| name.eq_ignore_ascii_case(&candidate))
        .map(|name| name.as_str())
}

fn manifest_key_for(manifest: &Option<AccountManifest>, candidate: &str) -> String {
    manifest
        .as_ref()
        .and_then(|manifest| {
            manifest
                .folders
                .keys()
                .find(|name| name.eq_ignore_ascii_case(candidate))
                .cloned()
        })
        .unwrap_or_else(|| candidate.to_string())
}

fn special_use_for(manifest: &Option<AccountManifest>, folder_key: &str) -> Option<SpecialUse> {
    let manifest = manifest.as_ref()?;
    let folder = manifest.folders.get(folder_key)?;
    let label = folder.special_use.as_deref()?;
    match label {
        "Enviados" => Some(SpecialUse::Sent),
        "Borradores" => Some(SpecialUse::Drafts),
        "Papelera" => Some(SpecialUse::Trash),
        "Spam" => Some(SpecialUse::Junk),
        "Archivo" => Some(SpecialUse::Archive),
        "Todos" => Some(SpecialUse::All),
        "Destacados" => Some(SpecialUse::Flagged),
        _ => None,
    }
}

/// Flags y fecha de un mensaje según el manifiesto (por `Message-ID` si se conoce,
/// o por el nombre de archivo si el manifiesto guarda esa ruta).
fn manifest_metadata(
    entries: &ManifestIndex,
    folder_key: &str,
    message_id: Option<&str>,
) -> (Vec<String>, Option<String>, Option<String>) {
    let found = message_id
        .and_then(|id| entries.get(&connect::normalize_message_id(id)))
        .filter(|(folder, _)| folder.eq_ignore_ascii_case(folder_key))
        .map(|(_, entry)| entry.clone());
    match found {
        Some(entry) => (entry.flags, entry.internal_date, entry.message_id),
        None => (Vec::new(), None, message_id.map(|id| id.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "imapb-restore-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx() -> (Ctx, Arc<crate::events::Logger>) {
        let logger = Arc::new(crate::events::Logger::with_capacity(200));
        (
            Ctx::headless(Arc::clone(&logger), crate::cancel::CancelToken::new()),
            logger,
        )
    }

    #[test]
    fn index_of_eml_tree_uses_deterministic_folder_names() {
        let dir = temp_dir("eml");
        let account = dir.join("midominio.com").join("a@midominio.com");
        std::fs::create_dir_all(account.join("INBOX").join("Clientes")).unwrap();
        std::fs::write(account.join("INBOX").join("1_a.eml"), b"Subject: A\r\n\r\nx").unwrap();
        std::fs::write(
            account.join("INBOX").join("Clientes").join("2_b.eml"),
            b"Subject: B\r\n\r\nx",
        )
        .unwrap();
        std::fs::write(account.join("_imap-backup-manifest.json"), b"{}").unwrap();

        let (ctx, _logger) = ctx();
        let index = build_index(&ctx, &account).unwrap();
        let names: Vec<String> = index.folders.iter().map(|f| f.remote_name.clone()).collect();
        assert_eq!(names, vec!["INBOX".to_string(), "INBOX/Clientes".to_string()]);
        assert_eq!(index.total_messages, 2);
        assert_eq!(index.kind, SourceKind::EmlDir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_of_mbox_and_maildir_and_zip() {
        let dir = temp_dir("mixed");

        // mbox
        let mbox_dir = dir.join("mbox");
        std::fs::create_dir_all(&mbox_dir).unwrap();
        {
            let mut writer = crate::formats::mbox::MboxWriter::create(&mbox_dir.join("INBOX.mbox")).unwrap();
            writer.append(b"Subject: Uno\r\n\r\ncuerpo", "a@b.com", None).unwrap();
            writer.append(b"Subject: Dos\r\n\r\ncuerpo", "a@b.com", None).unwrap();
            writer.finish().unwrap();
        }
        // maildir
        let maildir_dir = dir.join("maildir").join("Enviados");
        let writer = maildir::MaildirWriter::open(&maildir_dir).unwrap();
        writer
            .append(5, b"Subject: Tres\r\nMessage-ID: <tres@x>\r\n\r\ncuerpo", &["\\Seen".to_string()])
            .unwrap();

        let (ctx, _logger) = ctx();
        let mbox_index = build_index(&ctx, &mbox_dir).unwrap();
        assert_eq!(mbox_index.kind, SourceKind::Mbox);
        assert_eq!(mbox_index.total_messages, 2);
        assert_eq!(mbox_index.folders[0].remote_name, "INBOX");

        let maildir_index = build_index(&ctx, &dir.join("maildir")).unwrap();
        assert_eq!(maildir_index.kind, SourceKind::Maildir);
        assert_eq!(maildir_index.folders[0].remote_name, "Enviados");
        assert_eq!(maildir_index.folders[0].items[0].flags, vec!["\\Seen".to_string()]);

        // ZIP con la misma estructura
        let zip_path = dir.join("backup.zip");
        crate::archive::zip_directory(&mbox_dir, &zip_path, crate::config::ZipCompression::Deflate, &mut |_, _| {})
            .unwrap();
        let zip_index = build_index(&ctx, &zip_path).unwrap();
        assert_eq!(zip_index.kind, SourceKind::Zip);
        assert_eq!(zip_index.total_messages, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reader_streams_bytes_from_each_locator() {
        let dir = temp_dir("reader");
        std::fs::write(dir.join("uno.eml"), b"Subject: Uno\r\n\r\ncuerpo uno").unwrap();
        let mbox_path = dir.join("caja.mbox");
        {
            let mut writer = crate::formats::mbox::MboxWriter::create(&mbox_path).unwrap();
            writer.append(b"Subject: Dos\r\n\r\ncuerpo dos", "a@b.com", None).unwrap();
            writer.finish().unwrap();
        }

        let mut reader = SourceReader::open(&dir).unwrap();
        let file_bytes = reader
            .read(&Locator::File(dir.join("uno.eml")))
            .unwrap();
        assert!(String::from_utf8_lossy(&file_bytes).contains("cuerpo uno"));
        let mbox_bytes = reader
            .read(&Locator::Mbox {
                path: mbox_path.clone(),
                index: 0,
            })
            .unwrap();
        assert!(String::from_utf8_lossy(&mbox_bytes).contains("cuerpo dos"));

        // ZIP por índice
        let zip_path = dir.join("x.zip");
        crate::archive::zip_directory(&dir, &zip_path, crate::config::ZipCompression::Deflate, &mut |_, _| {})
            .unwrap();
        let mut zip_reader = SourceReader::open(&zip_path).unwrap();
        let zip_source = ZipSource::open(&zip_path).unwrap();
        let target = zip_source
            .entries()
            .iter()
            .find(|e| e.name.ends_with("uno.eml"))
            .unwrap()
            .index;
        let bytes = zip_reader.read(&Locator::Zip(target)).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("cuerpo uno"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignored_files_are_not_messages() {
        assert!(is_ignored_file("_imap-backup-manifest.json"));
        assert!(is_ignored_file("reporte.csv"));
        assert!(is_ignored_file("1_a.eml.1234.part"));
        assert!(is_ignored_file("INBOX/tmp/abc"));
        assert!(!is_ignored_file("INBOX/1_a.eml"));
        assert!(!is_ignored_file("INBOX.mbox"));
    }

    #[test]
    fn manifest_metadata_matches_by_message_id() {
        let mut manifest = AccountManifest::new("a@b.com", "h", "Eml");
        let folder = manifest.folder_mut("INBOX");
        folder.upsert(MessageEntry {
            uid: 1,
            message_id: Some("<ABC@x>".to_string()),
            size: 10,
            internal_date: Some("2024-05-06T07:08:09+00:00".to_string()),
            flags: vec!["\\Seen".to_string()],
            file: "1_abc.eml".to_string(),
            sha256: None,
        });
        let entries = index_manifest_entries(&manifest);
        let (flags, date, id) = manifest_metadata(&entries, "INBOX", Some("<abc@x>"));
        assert_eq!(flags, vec!["\\Seen".to_string()]);
        assert_eq!(date.unwrap(), "2024-05-06T07:08:09+00:00");
        assert_eq!(id.unwrap(), "<ABC@x>");
        assert!(special_use_for(&Some(manifest), "INBOX").is_none());
    }

    #[test]
    fn missing_source_is_an_error() {
        let (ctx, _logger) = ctx();
        let err = build_index(&ctx, Path::new("/ruta/que/no/existe")).unwrap_err();
        assert!(err.to_string().contains("no existe"));
    }
}
