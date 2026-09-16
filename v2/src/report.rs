// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

use serde::{Deserialize, Serialize};

/// Reporte estructurado de una corrida. Es el "recibo verificable" de cada operación:
/// se puede escribir en JSON/CSV, mostrar en la UI y comparar entre ejecuciones.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunReport {
    pub task: String,
    pub started_at: String,
    pub finished_at: String,
    pub elapsed_secs: f64,
    pub cancelled: bool,
    /// Bytes estimados antes de empezar (suma de RFC822.SIZE) y espacio libre disponible.
    pub estimated_bytes: Option<u64>,
    pub free_space_before: Option<u64>,
    pub accounts: Vec<AccountReport>,
    pub totals: Totals,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Totals {
    pub accounts: usize,
    pub accounts_with_errors: usize,
    pub messages: u64,
    pub new: u64,
    pub skipped: u64,
    pub uploaded: u64,
    pub bytes: u64,
    pub errors: usize,
    pub verified_folders: usize,
    pub unverified_folders: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountReport {
    pub account: String,
    pub host: String,
    pub elapsed_secs: f64,
    pub folders: Vec<FolderReport>,
    pub new: u64,
    pub skipped: u64,
    pub uploaded: u64,
    pub bytes: u64,
    pub messages_total: u64,
    pub errors: usize,
    pub verified: bool,
    pub zip_paths: Vec<String>,
    pub mbox_paths: Vec<String>,
    pub error_list: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FolderReport {
    pub folder: String,
    pub remote_name: Option<String>,
    /// Mensajes que el servidor reporta para la carpeta (EXISTS al momento del SELECT).
    pub server_count: Option<u64>,
    /// Mensajes presentes localmente después de la operación.
    pub local_count: u64,
    pub messages: u64,
    pub new: u64,
    pub skipped: u64,
    pub uploaded: u64,
    pub bytes: u64,
    pub errors: usize,
    /// Archivos locales que faltaban o estaban incompletos y se volvieron a bajar.
    pub repaired: u64,
    /// true si el conteo local coincide con el del servidor.
    pub verified: bool,
    /// true si la carpeta se recreó en el destino.
    pub created: bool,
    /// true si UIDVALIDITY cambió y se forzó una resincronización completa.
    pub invalidated: bool,
    pub resumed_from_uid: Option<u32>,
    pub errors_detail: Vec<String>,
}

impl AccountReport {
    pub fn new(account: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            account: account.into(),
            host: host.into(),
            ..Default::default()
        }
    }

    pub fn recompute(&mut self) {
        self.new = self.folders.iter().map(|f| f.new).sum();
        self.skipped = self.folders.iter().map(|f| f.skipped).sum();
        self.uploaded = self.folders.iter().map(|f| f.uploaded).sum();
        self.messages_total = self.folders.iter().map(|f| f.messages).sum();
        self.errors = self.folders.iter().map(|f| f.errors).sum();
        self.bytes = self.folders.iter().map(|f| f.bytes).sum();
        self.verified = !self.folders.is_empty()
            && self.folders.iter().all(|f| f.verified)
            && self.errors == 0;
    }
}

impl RunReport {
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            started_at: chrono::Local::now().to_rfc3339(),
            ..Default::default()
        }
    }

    pub fn finish(&mut self, elapsed_secs: f64, cancelled: bool) {
        self.elapsed_secs = elapsed_secs;
        self.cancelled = cancelled;
        self.finished_at = chrono::Local::now().to_rfc3339();
        self.totals = Totals {
            accounts: self.accounts.len(),
            accounts_with_errors: self.accounts.iter().filter(|a| a.errors > 0).count(),
            messages: self.accounts.iter().map(|a| a.messages_total).sum(),
            new: self.accounts.iter().map(|a| a.new).sum(),
            skipped: self.accounts.iter().map(|a| a.skipped).sum(),
            uploaded: self.accounts.iter().map(|a| a.uploaded).sum(),
            bytes: self.accounts.iter().map(|a| a.bytes).sum(),
            errors: self.accounts.iter().map(|a| a.errors).sum(),
            verified_folders: self
                .accounts
                .iter()
                .flat_map(|a| a.folders.iter())
                .filter(|f| f.verified)
                .count(),
            unverified_folders: self
                .accounts
                .iter()
                .flat_map(|a| a.folders.iter())
                .filter(|f| !f.verified)
                .count(),
        };
    }

    pub fn has_errors(&self) -> bool {
        self.totals.errors > 0 || self.totals.accounts_with_errors > 0
    }

    pub fn all_verified(&self) -> bool {
        self.totals.unverified_folders == 0 && self.totals.verified_folders > 0
    }

    pub fn summary_line(&self) -> String {
        let mb = self.totals.bytes as f64 / (1024.0 * 1024.0);
        let state = if self.cancelled {
            "cancelado"
        } else if self.has_errors() {
            "con errores"
        } else {
            "sin errores"
        };
        format!(
            "{}: {} cuenta(s), {} mensaje(s), {} nuevos, {} omitidos, {} subidos, {:.2} MB, {} error(es), verificado {}/{} carpetas, {:.1}s ({})",
            self.task,
            self.totals.accounts,
            self.totals.messages,
            self.totals.new,
            self.totals.skipped,
            self.totals.uploaded,
            mb,
            self.totals.errors,
            self.totals.verified_folders,
            self.totals.verified_folders + self.totals.unverified_folders,
            self.elapsed_secs,
            state
        )
    }

    pub fn to_json(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// CSV con una fila por carpeta. Todas las filas tienen exactamente 13 columnas
    /// (las que declara la cabecera), incluso cuando una cuenta no tiene carpetas.
    pub fn to_csv(&self) -> String {
        const HEADER: [&str; 13] = [
            "cuenta",
            "host",
            "formato_destino",
            "carpeta",
            "mensajes",
            "nuevos",
            "omitidos",
            "subidos",
            "bytes",
            "errores",
            "conteo_servidor",
            "conteo_local",
            "verificado",
        ];
        let mut out = HEADER.join(",");
        out.push('\n');

        for acc in &self.accounts {
            if acc.folders.is_empty() {
                let row = [
                    csv_escape(&acc.account),
                    csv_escape(&acc.host),
                    String::new(),
                    String::new(),
                    "0".to_string(),
                    "0".to_string(),
                    "0".to_string(),
                    "0".to_string(),
                    "0".to_string(),
                    acc.errors.to_string(),
                    String::new(),
                    String::new(),
                    acc.verified.to_string(),
                ];
                out.push_str(&row.join(","));
                out.push('\n');
                continue;
            }

            for f in &acc.folders {
                let row = [
                    csv_escape(&acc.account),
                    csv_escape(&acc.host),
                    csv_escape(f.remote_name.as_deref().unwrap_or(&f.folder)),
                    csv_escape(&f.folder),
                    f.messages.to_string(),
                    f.new.to_string(),
                    f.skipped.to_string(),
                    f.uploaded.to_string(),
                    f.bytes.to_string(),
                    f.errors.to_string(),
                    f.server_count.map(|v| v.to_string()).unwrap_or_default(),
                    f.local_count.to_string(),
                    f.verified.to_string(),
                ];
                out.push_str(&row.join(","));
                out.push('\n');
            }
        }
        out
    }

    pub fn write_reports(&self, base_path: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
        let mut written = Vec::new();
        let json_path = base_path.with_extension("json");
        crate::fsutil::write_atomic(&json_path, self.to_json()?.as_bytes())?;
        written.push(json_path);

        let csv_path = base_path.with_extension("csv");
        crate::fsutil::write_atomic(&csv_path, self.to_csv().as_bytes())?;
        written.push(csv_path);
        Ok(written)
    }
}

fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_escapes_commas_and_quotes() {
        assert_eq!(csv_escape("hola"), "hola");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("dijo \"hola\""), "\"dijo \"\"hola\"\"\"");
    }

    #[test]
    fn totals_are_aggregated_and_verification_requires_every_folder() {
        let mut report = RunReport::new("backup");
        let mut acc = AccountReport::new("a@b.com", "imap.b.com");
        acc.folders.push(FolderReport {
            folder: "INBOX".into(),
            messages: 10,
            new: 4,
            skipped: 6,
            server_count: Some(10),
            local_count: 10,
            verified: true,
            ..Default::default()
        });
        acc.folders.push(FolderReport {
            folder: "Sent".into(),
            messages: 5,
            new: 5,
            verified: false,
            errors: 1,
            ..Default::default()
        });
        acc.recompute();
        assert_eq!(acc.new, 9);
        assert_eq!(acc.skipped, 6);
        assert_eq!(acc.errors, 1);
        assert!(!acc.verified);

        report.accounts.push(acc);
        report.finish(3.5, false);
        assert_eq!(report.totals.messages, 15);
        assert_eq!(report.totals.new, 9);
        assert_eq!(report.totals.verified_folders, 1);
        assert_eq!(report.totals.unverified_folders, 1);
        assert!(report.has_errors());
        assert!(!report.all_verified());
        assert!(report.summary_line().contains("15 mensaje(s)"));
    }

    #[test]
    fn csv_report_has_header_and_one_row_per_folder() {
        let mut report = RunReport::new("backup");
        let mut acc = AccountReport::new("a@b.com", "imap.b.com");
        acc.folders.push(FolderReport {
            folder: "INBOX".to_string(),
            remote_name: Some("INBOX".to_string()),
            messages: 2,
            server_count: Some(2),
            local_count: 2,
            verified: true,
            ..Default::default()
        });
        report.accounts.push(acc);
        let csv = report.to_csv();
        assert_eq!(csv.lines().count(), 2);
        assert!(csv.starts_with("cuenta,host,"));
        for line in csv.lines() {
            assert_eq!(line.matches(',').count() + 1, 13, "columnas en: {}", line);
        }
    }

    #[test]
    fn csv_rows_stay_aligned_when_account_has_no_folders() {
        let mut report = RunReport::new("backup");
        let mut acc = AccountReport::new("a@b.com", "imap.b.com");
        acc.errors = 1;
        report.accounts.push(acc);
        let csv = report.to_csv();
        let data_line = csv.lines().nth(1).unwrap();
        assert_eq!(data_line.matches(',').count() + 1, 13, "columnas en: {}", data_line);
    }
}
