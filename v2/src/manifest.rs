// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Manifiesto por cuenta: el índice que convierte el backup en algo incremental,
//! reanudable, verificable y fiel al restaurar.
//!
//! Se guarda junto a los mensajes (`_imap-backup-manifest.json`) y registra, por
//! carpeta: `UIDVALIDITY`, `UIDNEXT`, el nombre remoto original, la carpeta especial
//! y, por mensaje: UID, `Message-ID`, tamaño, fecha interna, flags, archivo destino
//! y (opcional) su hash SHA-256.
//!
//! Con esto:
//! * la sincronización incremental no necesita volver a bajar los cuerpos;
//! * un archivo truncado se detecta y se vuelve a descargar;
//! * un cambio de `UIDVALIDITY` fuerza resincronización en vez de colisionar;
//! * la restauración conserva fecha y flags reales;
//! * una corrida interrumpida continúa donde quedó.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

pub const MANIFEST_FILE: &str = "_imap-backup-manifest.json";
pub const MANIFEST_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountManifest {
    pub version: u32,
    pub email: String,
    pub host: String,
    /// Formato en el que se escribieron los mensajes (`Eml`, `Mbox`, `Maildir`).
    #[serde(default)]
    pub format: String,
    pub created_at: String,
    pub updated_at: String,
    /// Clave: nombre remoto original de la carpeta.
    pub folders: BTreeMap<String, FolderManifest>,
}

impl AccountManifest {
    pub fn new(email: impl Into<String>, host: impl Into<String>, format: impl Into<String>) -> Self {
        let now = chrono::Local::now().to_rfc3339();
        Self {
            version: MANIFEST_VERSION,
            email: email.into(),
            host: host.into(),
            format: format.into(),
            created_at: now.clone(),
            updated_at: now,
            folders: BTreeMap::new(),
        }
    }

    pub fn path_for(account_dir: &Path) -> PathBuf {
        account_dir.join(MANIFEST_FILE)
    }

    pub fn load(account_dir: &Path) -> Option<Self> {
        let path = Self::path_for(account_dir);
        let content = std::fs::read_to_string(&path).ok()?;
        let manifest: Self = serde_json::from_str(&content).ok()?;
        if manifest.version != MANIFEST_VERSION {
            return None;
        }
        Some(manifest)
    }

    pub fn save(&mut self, account_dir: &Path) -> Result<()> {
        self.updated_at = chrono::Local::now().to_rfc3339();
        let path = Self::path_for(account_dir);
        let json = serde_json::to_string_pretty(self)?;
        crate::fsutil::write_atomic(&path, json.as_bytes())?;
        Ok(())
    }

    pub fn folder(&self, remote_name: &str) -> Option<&FolderManifest> {
        self.folders.get(remote_name)
    }

    pub fn folder_mut(&mut self, remote_name: &str) -> &mut FolderManifest {
        self.folders
            .entry(remote_name.to_string())
            .or_insert_with(|| FolderManifest::new(remote_name))
    }

    /// Mensajes registrados de una carpeta (0 si no existe en el manifiesto).
    pub fn folder_message_count(&self, remote_name: &str) -> u64 {
        self.folders
            .get(remote_name)
            .map(|f| f.messages.len() as u64)
            .unwrap_or(0)
    }

    pub fn total_messages(&self) -> u64 {
        self.folders.values().map(|f| f.messages.len() as u64).sum()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderManifest {
    pub remote_name: String,
    pub delimiter: Option<char>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub special_use: Option<String>,
    /// 0 = desconocido (el servidor no lo informó).
    #[serde(default)]
    pub uid_validity: u32,
    #[serde(default)]
    pub uid_next: u32,
    #[serde(default)]
    pub server_count: u64,
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub messages: Vec<MessageEntry>,
}

impl FolderManifest {
    pub fn new(remote_name: impl Into<String>) -> Self {
        Self {
            remote_name: remote_name.into(),
            delimiter: None,
            special_use: None,
            uid_validity: 0,
            uid_next: 0,
            server_count: 0,
            run_id: String::new(),
            messages: Vec::new(),
        }
    }

    pub fn max_uid(&self) -> Option<u32> {
        self.messages.iter().map(|m| m.uid).max()
    }

    pub fn entry_by_uid(&self, uid: u32) -> Option<&MessageEntry> {
        self.messages.iter().find(|m| m.uid == uid)
    }

    pub fn upsert(&mut self, entry: MessageEntry) {
        match self.messages.iter_mut().find(|m| m.uid == entry.uid) {
            Some(existing) => *existing = entry,
            None => self.messages.push(entry),
        }
        self.messages.sort_by_key(|m| m.uid);
    }

    pub fn total_bytes(&self) -> u64 {
        self.messages.iter().map(|m| m.size).sum()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageEntry {
    pub uid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default)]
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal_date: Option<String>,
    #[serde(default)]
    pub flags: Vec<String>,
    /// Ruta del mensaje relativa al directorio de la cuenta.
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Metadatos de un mensaje tal como los reporta el servidor (`UID FETCH` sin cuerpo).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerMessage {
    pub uid: u32,
    pub size: u64,
    pub message_id: Option<String>,
    pub internal_date: Option<String>,
    pub flags: Vec<String>,
}

/// Qué rango de metadatos hay que pedir al servidor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataFetch {
    /// Nada: el manifiesto ya cubre la carpeta y `UIDNEXT` no cambió.
    Nothing,
    /// Solo desde este UID en adelante (mensajes nuevos).
    Since(u32),
    /// Rango completo `1:*`.
    All,
}

/// Decide si se puede evitar el barrido completo de metadatos.
///
/// Es la diferencia central frente a la v1: si la carpeta no cambió desde la última
/// corrida, no se emite ningún `FETCH`. Si solo llegaron mensajes nuevos, se pide
/// `UID NEXT:*`. Y si cambió el `UIDVALIDITY` (buzón recreado) se fuerza un barrido
/// completo, porque los UIDs anteriores ya no significan nada.
pub fn plan_metadata_fetch(
    prev: Option<&FolderManifest>,
    uid_validity: u32,
    uid_next: u32,
) -> MetadataFetch {
    let Some(prev) = prev else {
        return MetadataFetch::All;
    };
    if prev.messages.is_empty() {
        return MetadataFetch::All;
    }
    if prev.uid_validity != 0 && uid_validity != 0 && prev.uid_validity != uid_validity {
        return MetadataFetch::All;
    }
    if prev.uid_next != 0 && uid_next != 0 && prev.uid_next == uid_next {
        return MetadataFetch::Nothing;
    }
    match prev.max_uid() {
        Some(max) if max > 0 => MetadataFetch::Since(max.saturating_add(1)),
        _ => MetadataFetch::All,
    }
}

#[derive(Debug, Clone, Default)]
pub struct SyncPlan {
    /// El `UIDVALIDITY` cambió: el manifiesto previo de la carpeta se descarta.
    pub invalidated: bool,
    pub to_download: Vec<ServerMessage>,
    pub download_bytes: u64,
    /// Mensajes ya presentes y con tamaño correcto.
    pub unchanged: u64,
    /// Mensajes cuyo archivo local falta o está incompleto: se vuelven a bajar.
    pub repaired: u64,
    /// Entradas locales cuyo UID ya no existe en el servidor.
    pub orphans: u64,
    /// Entradas cuyo contenido ya está bien pero cuyos flags cambiaron en el
    /// servidor: se actualizan en el manifiesto sin volver a descargar el mensaje.
    pub flag_updates: Vec<MessageEntry>,
    pub total_messages: u64,
}

impl SyncPlan {
    pub fn needs_download(&self) -> bool {
        !self.to_download.is_empty()
    }

    /// UIDs del plan en notación de conjunto de secuencias IMAP (`1,2,5:9`).
    pub fn uid_set(&self) -> String {
        let uids: Vec<u32> = self.to_download.iter().map(|m| m.uid).collect();
        seq_set(&uids)
    }
}

/// Compacta una lista de UIDs en rangos: `[1,2,3,7,9,10]` → `"1:3,7,9:10"`.
pub fn seq_set(uids: &[u32]) -> String {
    let mut sorted: Vec<u32> = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out: Vec<String> = Vec::new();
    let mut index = 0usize;
    while index < sorted.len() {
        let start = sorted[index];
        let mut end = start;
        while index + 1 < sorted.len() && sorted[index + 1] == end + 1 {
            index += 1;
            end = sorted[index];
        }
        if start == end {
            out.push(start.to_string());
        } else {
            out.push(format!("{}:{}", start, end));
        }
        index += 1;
    }
    out.join(",")
}

/// Decide qué mensajes hace falta descargar, sin tocar la red.
///
/// `account_dir` se usa para comprobar que el archivo local siga existiendo y con el
/// tamaño esperado; un `.eml` truncado se marca para re-descarga en vez de darse por
/// bueno (comportamiento de la v1, que solo miraba `size >= 1`).
///
/// `check_file_size` debe ser `false` cuando varios mensajes comparten archivo (mbox),
/// porque ahí el tamaño del archivo no dice nada del tamaño de cada mensaje.
pub fn plan_sync(
    server: &[ServerMessage],
    prev: Option<&FolderManifest>,
    uid_validity: u32,
    account_dir: &Path,
    skip_existing: bool,
    check_file_size: bool,
) -> SyncPlan {
    let invalidated = prev
        .map(|p| {
            p.uid_validity != 0 && uid_validity != 0 && p.uid_validity != uid_validity
        })
        .unwrap_or(false);

    let previous: Option<&FolderManifest> = if invalidated { None } else { prev };
    let mut plan = SyncPlan {
        invalidated,
        total_messages: server.len() as u64,
        ..Default::default()
    };

    let mut server_uids: HashSet<u32> = HashSet::with_capacity(server.len());
    for message in server {
        server_uids.insert(message.uid);
    }

    for message in server {
        let existing = previous.and_then(|p| p.entry_by_uid(message.uid));
        let mut must_download = !skip_existing;

        if skip_existing {
            match existing {
                None => must_download = true,
                Some(entry) => {
                    let expected_size = if message.size > 0 {
                        message.size
                    } else {
                        entry.size
                    };
                    let file_path = account_dir.join(&entry.file);
                    let actual_size = crate::fsutil::file_len(&file_path);
                    let size_ok = match actual_size {
                        None => false,
                        Some(_) if !check_file_size => true,
                        Some(actual) => actual == expected_size && expected_size > 0,
                    };
                    if size_ok {
                        must_download = false;
                    } else {
                        must_download = true;
                        plan.repaired += 1;
                    }
                }
            }
        }

        if must_download {
            plan.download_bytes = plan.download_bytes.saturating_add(message.size);
            plan.to_download.push(message.clone());
        } else {
            plan.unchanged += 1;
            if let Some(entry) = existing {
                if !same_flags(&entry.flags, &message.flags) {
                    let mut updated = entry.clone();
                    updated.flags = message.flags.clone();
                    if message.message_id.is_some() {
                        updated.message_id = message.message_id.clone();
                    }
                    plan.flag_updates.push(updated);
                }
            }
        }
    }

    if let Some(previous) = previous {
        plan.orphans = previous
            .messages
            .iter()
            .filter(|m| !server_uids.contains(&m.uid))
            .count() as u64;
    }

    debug_assert_eq!(
        plan.to_download.len() as u64 + plan.unchanged,
        plan.total_messages
    );
    plan
}

/// Compara flags ignorando orden y mayúsculas (`\Seen` vs `\seen`).
fn same_flags(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let lower = |list: &[String]| {
        let mut out: Vec<String> = list.iter().map(|f| f.to_lowercase()).collect();
        out.sort();
        out
    };
    lower(a) == lower(b)
}

/// Hash SHA-256 en hexadecimal (verificación opcional de integridad local).
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("No se pudo leer para hashear: {}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_account(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "imapb-manifest-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(uid: u32, size: u64, file: &str) -> MessageEntry {
        MessageEntry {
            uid,
            message_id: Some(format!("<{}@test>", uid)),
            size,
            internal_date: Some("2024-01-02T03:04:05+00:00".to_string()),
            flags: vec!["\\Seen".to_string()],
            file: file.to_string(),
            sha256: None,
        }
    }

    #[test]
    fn seq_set_compacts_ranges() {
        assert_eq!(seq_set(&[1, 2, 3, 7, 9, 10]), "1:3,7,9:10");
        assert_eq!(seq_set(&[]), "");
        assert_eq!(seq_set(&[5]), "5");
        assert_eq!(seq_set(&[3, 1, 2, 2]), "1:3");
    }

    #[test]
    fn metadata_fetch_skips_work_when_nothing_changed() {
        let mut folder = FolderManifest::new("INBOX");
        folder.uid_validity = 100;
        folder.uid_next = 51;
        folder.messages.push(entry(50, 100, "INBOX/50_a.eml"));
        assert_eq!(plan_metadata_fetch(Some(&folder), 100, 51), MetadataFetch::Nothing);
        assert_eq!(plan_metadata_fetch(Some(&folder), 100, 60), MetadataFetch::Since(51));
        // UIDVALIDITY distinto: barrido completo.
        assert_eq!(plan_metadata_fetch(Some(&folder), 200, 60), MetadataFetch::All);
        assert_eq!(plan_metadata_fetch(None, 100, 51), MetadataFetch::All);
    }

    #[test]
    fn plan_downloads_only_new_and_broken_messages() {
        let dir = temp_account("plan");
        std::fs::write(dir.join("50_a.eml"), vec![7u8; 100]).unwrap();
        std::fs::write(dir.join("51_b.eml"), vec![7u8; 10]).unwrap(); // truncado

        let mut folder = FolderManifest::new("INBOX");
        folder.uid_validity = 42;
        folder.messages.push(entry(50, 100, "50_a.eml"));
        folder.messages.push(entry(51, 500, "51_b.eml"));
        folder.messages.push(entry(52, 80, "52_c.eml")); // borrado en el servidor

        let server = vec![
            ServerMessage { uid: 50, size: 100, ..Default::default() },
            ServerMessage { uid: 51, size: 500, ..Default::default() },
            ServerMessage { uid: 53, size: 20, ..Default::default() },
        ];

        let plan = plan_sync(&server, Some(&folder), 42, &dir, true, true);
        assert!(!plan.invalidated);
        assert_eq!(plan.total_messages, 3);
        assert_eq!(plan.unchanged, 1);
        assert_eq!(plan.repaired, 1);
        assert_eq!(plan.orphans, 1);
        assert_eq!(plan.download_bytes, 520);
        assert_eq!(plan.uid_set(), "51,53");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_redownloads_everything_when_uidvalidity_changed() {
        let dir = temp_account("uidvalidity");
        std::fs::write(dir.join("1_a.eml"), vec![1u8; 10]).unwrap();
        let mut folder = FolderManifest::new("INBOX");
        folder.uid_validity = 1;
        folder.messages.push(entry(1, 10, "1_a.eml"));

        let server = vec![ServerMessage { uid: 1, size: 999, ..Default::default() }];
        let plan = plan_sync(&server, Some(&folder), 77, &dir, true, true);
        assert!(plan.invalidated);
        assert_eq!(plan.to_download.len(), 1);
        assert_eq!(plan.orphans, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_forces_download_when_skip_existing_is_disabled() {
        let dir = temp_account("no-skip");
        std::fs::write(dir.join("1_a.eml"), vec![1u8; 10]).unwrap();
        let mut folder = FolderManifest::new("INBOX");
        folder.uid_validity = 1;
        folder.messages.push(entry(1, 10, "1_a.eml"));
        let server = vec![ServerMessage { uid: 1, size: 10, ..Default::default() }];
        let plan = plan_sync(&server, Some(&folder), 1, &dir, false, true);
        assert_eq!(plan.to_download.len(), 1);
        assert_eq!(plan.unchanged, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_redownloads_when_local_file_is_missing() {
        let dir = temp_account("missing");
        let mut folder = FolderManifest::new("INBOX");
        folder.uid_validity = 1;
        folder.messages.push(entry(1, 10, "1_a.eml"));
        let server = vec![ServerMessage { uid: 1, size: 10, ..Default::default() }];
        let plan = plan_sync(&server, Some(&folder), 1, &dir, true, true);
        assert_eq!(plan.repaired, 1);
        assert_eq!(plan.to_download.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_roundtrips_and_upserts() {
        let dir = temp_account("roundtrip");
        let mut manifest = AccountManifest::new("a@b.com", "imap.b.com", "Eml");
        manifest.folder_mut("INBOX").uid_validity = 9;
        manifest.folder_mut("INBOX").upsert(entry(2, 20, "2_b.eml"));
        manifest.folder_mut("INBOX").upsert(entry(1, 10, "1_a.eml"));
        manifest.folder_mut("INBOX").upsert(entry(2, 25, "2_b.eml")); // actualiza
        manifest.save(&dir).unwrap();

        let loaded = AccountManifest::load(&dir).unwrap();
        let inbox = loaded.folder("INBOX").unwrap();
        assert_eq!(inbox.uid_validity, 9);
        assert_eq!(inbox.messages.len(), 2);
        assert_eq!(inbox.messages[0].uid, 1);
        assert_eq!(inbox.messages[1].size, 25);
        assert_eq!(loaded.total_messages(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_rejects_other_versions() {
        let dir = temp_account("version");
        let path = AccountManifest::path_for(&dir);
        std::fs::write(&path, r#"{"version":1,"email":"a","host":"b","created_at":"","updated_at":"","folders":{}}"#).unwrap();
        assert!(AccountManifest::load(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flag_changes_are_reported_without_downloading() {
        let dir = temp_account("flags");
        std::fs::write(dir.join("1_a.eml"), vec![1u8; 10]).unwrap();
        let mut folder = FolderManifest::new("INBOX");
        folder.uid_validity = 1;
        folder.messages.push(entry(1, 10, "1_a.eml"));

        // Mismos flags: sin cambios.
        let server = vec![ServerMessage {
            uid: 1,
            size: 10,
            flags: vec!["\\Seen".to_string()],
            ..Default::default()
        }];
        let plan = plan_sync(&server, Some(&folder), 1, &dir, true, true);
        assert!(plan.flag_updates.is_empty());

        // Flags distintos (y en otro orden): se actualiza el manifiesto.
        let server = vec![ServerMessage {
            uid: 1,
            size: 10,
            flags: vec!["\\Flagged".to_string(), "\\Seen".to_string()],
            ..Default::default()
        }];
        let plan = plan_sync(&server, Some(&folder), 1, &dir, true, true);
        assert_eq!(plan.unchanged, 1);
        assert!(!plan.needs_download());
        assert_eq!(plan.flag_updates.len(), 1);
        assert_eq!(plan.flag_updates[0].flags.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mbox_entries_trust_file_existence_not_size() {
        // En formato mbox todos los mensajes de una carpeta comparten archivo: el
        // tamaño del archivo no debe interpretarse como el tamaño del mensaje.
        let dir = temp_account("mbox-shared-file");
        std::fs::write(dir.join("INBOX.mbox"), vec![9u8; 5000]).unwrap();
        let mut folder = FolderManifest::new("INBOX");
        folder.uid_validity = 1;
        folder.messages.push(MessageEntry {
            file: "INBOX.mbox".to_string(),
            ..entry(1, 120, "INBOX.mbox")
        });
        let server = vec![ServerMessage { uid: 1, size: 120, ..Default::default() }];

        let with_size_check = plan_sync(&server, Some(&folder), 1, &dir, true, true);
        assert_eq!(with_size_check.repaired, 1);

        let without_size_check = plan_sync(&server, Some(&folder), 1, &dir, true, false);
        assert_eq!(without_size_check.repaired, 0);
        assert_eq!(without_size_check.unchanged, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
