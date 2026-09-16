// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Verificación local de un backup: recorre el manifiesto y comprueba que cada
//! mensaje registrado siga en disco, con el tamaño esperado y (si se activó
//! `hash_verify` al respaldar) el mismo SHA-256.
//!
//! Es la respuesta a "¿puedo confiar en este backup?" sin necesidad de tocar el
//! servidor. No descarga nada.

use crate::config::{AccountConfig, AppConfig};
use crate::events::Ctx;
use crate::folders;
use crate::manifest::{self, AccountManifest};
use crate::report::{AccountReport, FolderReport, RunReport};
use anyhow::Result;
use std::path::Path;
use std::time::Instant;

#[derive(Debug, Clone, Default)]
pub struct VerifyLocalSummary {
    pub folders: u64,
    pub messages: u64,
    pub missing: u64,
    pub size_mismatch: u64,
    pub hash_mismatch: u64,
    pub bytes: u64,
}

impl VerifyLocalSummary {
    pub fn is_healthy(&self) -> bool {
        self.missing == 0 && self.size_mismatch == 0 && self.hash_mismatch == 0
    }
}

/// Verifica el backup local de una cuenta ya resabiada.
pub fn verify_account(
    account: &AccountConfig,
    output_dir: &std::path::Path,
    ctx: &Ctx,
) -> Result<(AccountReport, VerifyLocalSummary)> {
    let label = account.get_domain_or_label();
    let account_dir = folders::account_dir(output_dir, &label, &account.email);
    verify_dir(&account.email, &account.host, &account_dir, ctx)
}

/// Verifica el backup de una cuenta a partir de su carpeta real (permite verificar
/// un backup del que no tenemos la configuración).
pub fn verify_dir(
    email: &str,
    host: &str,
    account_dir: &Path,
    ctx: &Ctx,
) -> Result<(AccountReport, VerifyLocalSummary)> {
    let scope = email.to_string();
    let mut report = AccountReport::new(email, host);
    let mut summary = VerifyLocalSummary::default();

    let Some(manifest) = AccountManifest::load(account_dir) else {
        anyhow::bail!(
            "No se encontró el manifiesto en {}: ¿esa cuenta ya fue respaldada?",
            account_dir.display()
        );
    };
    ctx.info(
        &scope,
        format!(
            "Verificando {} carpeta(s) / {} mensaje(s) registrados en {}",
            manifest.folders.len(),
            manifest.total_messages(),
            account_dir.display()
        ),
    );

    for (folder_name, folder) in &manifest.folders {
        let mut folder_report = FolderReport {
            folder: folder_name.clone(),
            remote_name: Some(folder_name.clone()),
            messages: folder.messages.len() as u64,
            ..Default::default()
        };

        for entry in &folder.messages {
            let path = account_dir.join(&entry.file);
            summary.messages += 1;
            match crate::fsutil::file_len(&path) {
                None => {
                    summary.missing += 1;
                    folder_report.errors += 1;
                    folder_report
                        .errors_detail
                        .push(format!("falta el archivo {}", entry.file));
                }
                Some(length) => {
                    summary.bytes += length;
                    // En mbox el archivo es compartido: solo se comprueba que exista.
                    let size_known = entry.size > 0;
                    if size_known && length != entry.size {
                        summary.size_mismatch += 1;
                        folder_report.errors += 1;
                        folder_report.errors_detail.push(format!(
                            "{}: tamaño {} ≠ esperado {}",
                            entry.file, length, entry.size
                        ));
                    }
                    if let Some(expected) = &entry.sha256 {
                        match manifest::sha256_file(&path) {
                            Ok(actual) if actual.eq_ignore_ascii_case(expected) => {}
                            Ok(_) => {
                                summary.hash_mismatch += 1;
                                folder_report.errors += 1;
                                folder_report
                                    .errors_detail
                                    .push(format!("{}: hash SHA-256 distinto", entry.file));
                            }
                            Err(err) => {
                                folder_report.errors += 1;
                                folder_report
                                    .errors_detail
                                    .push(format!("{}: no se pudo hashear ({})", entry.file, err));
                            }
                        }
                    }
                }
            }
        }

        folder_report.verified = folder_report.errors == 0;
        folder_report.local_count = folder.messages.len() as u64;
        summary.folders += 1;
        report.folders.push(folder_report);
    }

    report.recompute();
    if summary.is_healthy() {
        ctx.success(
            &scope,
            format!(
                "Verificación correcta: {} mensaje(s) en {} carpeta(s) ({}).",
                summary.messages,
                summary.folders,
                crate::fsutil::human_bytes(summary.bytes)
            ),
        );
    } else {
        ctx.error(
            &scope,
            format!(
                "Verificación con problemas: {} faltante(s), {} con tamaño distinto, {} con hash distinto.",
                summary.missing, summary.size_mismatch, summary.hash_mismatch
            ),
        );
    }

    Ok((report, summary))
}

/// Verifica todas las cuentas: las de la configuración si existen, y si no, las
/// que se descubran dentro del directorio de backup (así `verify` sirve sin
/// configuración, que es justo cuando más se necesita).
pub fn verify_all(cfg: &AppConfig, output_dir: &Path, ctx: &Ctx) -> Result<RunReport> {
    let started = Instant::now();
    let mut report = RunReport::new("Verificación local");

    // (email, host/label, carpeta de la cuenta)
    let mut targets: Vec<(String, String, std::path::PathBuf)> = Vec::new();
    for account in &cfg.accounts {
        let label = account.get_domain_or_label();
        let dir = folders::account_dir(output_dir, &label, &account.email);
        targets.push((account.email.clone(), account.host.clone(), dir));
    }
    if targets.is_empty() {
        for (label, email, dir) in discover_account_dirs(output_dir) {
            targets.push((email, label, dir));
        }
    }

    if targets.is_empty() {
        let message = format!(
            "No se encontró ningún backup (carpetas con {}) dentro de {}.",
            manifest::MANIFEST_FILE,
            output_dir.display()
        );
        ctx.error("verify", &message);
        let mut account_report = AccountReport::new("—", "");
        account_report.errors += 1;
        account_report.error_list.push(message);
        report.accounts.push(account_report);
        report.finish(started.elapsed().as_secs_f64(), false);
        return Ok(report);
    }

    if cfg.accounts.is_empty() {
        report.notes.push(format!(
            "Sin configuración: se verificaron las {} cuenta(s) encontradas en {}",
            targets.len(),
            output_dir.display()
        ));
    }

    for (email, host, dir) in targets {
        match verify_dir(&email, &host, &dir, ctx) {
            Ok((account_report, _summary)) => report.accounts.push(account_report),
            Err(err) => {
                ctx.error(&email, format!("No se pudo verificar: {:#}", err));
                let mut account_report = AccountReport::new(&email, &host);
                account_report.errors += 1;
                account_report.error_list.push(err.to_string());
                report.accounts.push(account_report);
            }
        }
    }

    report.finish(started.elapsed().as_secs_f64(), false);
    Ok(report)
}

/// Busca carpetas de cuenta (`…/etiqueta/email/_imap-backup-manifest.json`)
/// dentro de un directorio de backup y devuelve `(etiqueta, email, carpeta)`.
pub fn discover_account_dirs(output_dir: &Path) -> Vec<(String, String, std::path::PathBuf)> {
    let mut found: Vec<(String, String, std::path::PathBuf)> = Vec::new();
    for entry in walkdir::WalkDir::new(output_dir)
        .max_depth(4)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        if !entry.file_type().is_file() || entry.file_name() != manifest::MANIFEST_FILE {
            continue;
        }
        let Some(dir) = entry.path().parent() else {
            continue;
        };
        let email = dir
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if email.is_empty() {
            continue;
        }
        let label = dir
            .parent()
            .and_then(|parent| parent.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        found.push((label, email, dir.to_path_buf()));
    }
    found.sort();
    found.dedup_by(|a, b| a.2 == b.2);
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Logger;
    use crate::manifest::{AccountManifest, MessageEntry};
    use std::sync::Arc;

    fn test_ctx() -> Ctx {
        Ctx::headless(
            Arc::new(Logger::with_capacity(100)),
            crate::cancel::CancelToken::new(),
        )
    }

    #[test]
    fn detects_missing_and_truncated_files() {
        let base = std::env::temp_dir().join(format!("imapb-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut account = AccountConfig::new("a@midominio.com", "imap.midominio.com");
        account.label = Some("midominio.com".to_string());
        let account_dir = folders::account_dir(&base, "midominio.com", "a@midominio.com");
        std::fs::create_dir_all(account_dir.join("INBOX")).unwrap();
        std::fs::write(account_dir.join("INBOX").join("1_ok.eml"), vec![7u8; 20]).unwrap();
        std::fs::write(account_dir.join("INBOX").join("2_corto.eml"), vec![7u8; 5]).unwrap();

        let mut manifest = AccountManifest::new(&account.email, &account.host, "Eml");
        let folder = manifest.folder_mut("INBOX");
        folder.messages.push(MessageEntry {
            uid: 1,
            message_id: Some("<1@x>".into()),
            size: 20,
            internal_date: None,
            flags: vec![],
            file: "INBOX/1_ok.eml".into(),
            sha256: None,
        });
        folder.messages.push(MessageEntry {
            uid: 2,
            message_id: Some("<2@x>".into()),
            size: 20,
            internal_date: None,
            flags: vec![],
            file: "INBOX/2_corto.eml".into(),
            sha256: None,
        });
        folder.messages.push(MessageEntry {
            uid: 3,
            message_id: Some("<3@x>".into()),
            size: 20,
            internal_date: None,
            flags: vec![],
            file: "INBOX/3_borrado.eml".into(),
            sha256: None,
        });
        manifest.save(&account_dir).unwrap();

        let (report, summary) = verify_account(&account, &base, &test_ctx()).unwrap();
        assert_eq!(summary.messages, 3);
        assert_eq!(summary.missing, 1);
        assert_eq!(summary.size_mismatch, 1);
        assert!(!summary.is_healthy());
        assert_eq!(report.errors, 2);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn detects_hash_mismatch_when_recorded() {
        let base = std::env::temp_dir().join(format!("imapb-verify-hash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let account = AccountConfig::new("b@midominio.com", "imap.midominio.com");
        let account_dir = folders::account_dir(&base, "midominio.com", "b@midominio.com");
        std::fs::create_dir_all(account_dir.join("INBOX")).unwrap();
        std::fs::write(account_dir.join("INBOX").join("1_a.eml"), b"contenido").unwrap();

        let mut manifest = AccountManifest::new(&account.email, &account.host, "Eml");
        let folder = manifest.folder_mut("INBOX");
        folder.messages.push(MessageEntry {
            uid: 1,
            message_id: None,
            size: 9,
            internal_date: None,
            flags: vec![],
            file: "INBOX/1_a.eml".into(),
            sha256: Some(manifest::sha256_hex(b"otro contenido")),
        });
        manifest.save(&account_dir).unwrap();

        let (_report, summary) = verify_account(&account, &base, &test_ctx()).unwrap();
        assert_eq!(summary.hash_mismatch, 1);
        let _ = std::fs::remove_dir_all(&base);
    }
}
