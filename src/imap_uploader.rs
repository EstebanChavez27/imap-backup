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

use crate::state::{LogEntry, RestoreEvent, RestoreSummary};
use anyhow::{bail, Context, Result};
use native_tls::TlsConnector;
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::net::TcpStream;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};
use walkdir::WalkDir;
use zip::ZipArchive;

#[allow(dead_code)]
pub struct RestoreItem {
    pub folder: String,
    pub eml_bytes: Vec<u8>,
    pub file_name: String,
}

pub struct ImapUploader {
    host: String,
    port: u16,
    email: String,
    password: String,
    timeout_secs: u64,
    retry_attempts: u32,
    retry_delay_secs: u64,
    event_sender: Option<Sender<RestoreEvent>>,
}

impl ImapUploader {
    pub fn new(
        host: String,
        port: u16,
        email: String,
        password: String,
        timeout_secs: u64,
        retry_attempts: u32,
        retry_delay_secs: u64,
        event_sender: Option<Sender<RestoreEvent>>,
    ) -> Self {
        Self {
            host: host.trim().replace(['\r', '\n'], ""),
            port,
            email: email.trim().replace(['\r', '\n'], ""),
            password: password.replace(['\r', '\n'], ""),
            timeout_secs,
            retry_attempts,
            retry_delay_secs,
            event_sender,
        }
    }

    fn emit_log(&self, entry: LogEntry) {
        if let Some(ref tx) = self.event_sender {
            let _ = tx.send(RestoreEvent::Log(entry));
        }
    }

    fn emit_progress(&self, folder: &str, current: u64, total: u64, bytes: u64) {
        if let Some(ref tx) = self.event_sender {
            let _ = tx.send(RestoreEvent::Progress {
                folder: folder.to_string(),
                current,
                total,
                bytes,
            });
        }
    }

    /// Conecta y se autentica vía TLS al servidor IMAP de destino
    fn connect_and_login(&self) -> Result<imap::Session<native_tls::TlsStream<TcpStream>>> {
        let tls_builder = TlsConnector::builder();
        let tls = tls_builder
            .build()
            .with_context(|| "Error inicializando TLS")?;

        let tcp_stream = TcpStream::connect((self.host.as_str(), self.port))
            .with_context(|| format!("No se pudo conectar vía TCP a {}:{}", self.host, self.port))?;

        let timeout = Duration::from_secs(self.timeout_secs);
        tcp_stream.set_read_timeout(Some(timeout))?;
        tcp_stream.set_write_timeout(Some(timeout))?;

        let tls_stream = tls
            .connect(&self.host, tcp_stream)
            .with_context(|| format!("Error en handshake TLS con {}", self.host))?;

        let client = imap::Client::new(tls_stream);

        let session = client
            .login(&self.email, &self.password)
            .map_err(|(e, _)| anyhow::anyhow!("Fallo de autenticación en destino IMAP: {}", e))?;

        Ok(session)
    }

    /// Analiza el origen de backup (.zip o carpeta) y agrupa los correos por carpeta
    pub fn scan_backup_source(source_path: &Path) -> Result<BTreeMap<String, Vec<RestoreItem>>> {
        if !source_path.exists() {
            bail!("La ruta de origen no existe: {}", source_path.display());
        }

        let mut map: BTreeMap<String, Vec<RestoreItem>> = BTreeMap::new();

        if source_path.is_file() {
            // Origen es un archivo ZIP
            let file = File::open(source_path)
                .with_context(|| format!("No se pudo abrir el archivo ZIP: {}", source_path.display()))?;
            let mut archive = ZipArchive::new(file)
                .with_context(|| "El archivo seleccionado no es un archivo ZIP válido")?;

            for i in 0..archive.len() {
                let mut zip_file = match archive.by_index(i) {
                    Ok(f) => f,
                    Err(_) => continue,
                };

                let name = zip_file.name().to_string();
                if zip_file.is_dir() || !name.to_lowercase().ends_with(".eml") {
                    continue;
                }

                let mut bytes = Vec::new();
                if zip_file.read_to_end(&mut bytes).is_err() || bytes.is_empty() {
                    continue;
                }

                // Extraer nombre de carpeta del path relativo dentro del ZIP
                let path_obj = Path::new(&name);
                let file_name = path_obj
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();

                let folder = if let Some(parent) = path_obj.parent() {
                    let parent_str = parent.to_string_lossy().replace('\\', "/");
                    if parent_str.is_empty() {
                        "INBOX".to_string()
                    } else {
                        // Limpiar prefijos de cuenta si existían en el backup
                        clean_folder_path(&parent_str)
                    }
                } else {
                    "INBOX".to_string()
                };

                map.entry(folder.clone()).or_default().push(RestoreItem {
                    folder,
                    eml_bytes: bytes,
                    file_name,
                });
            }
        } else if source_path.is_dir() {
            // Origen es un directorio con archivos .eml
            for entry in WalkDir::new(source_path).into_iter().filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_file() && path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase() == "eml" {
                    let mut bytes = Vec::new();
                    if let Ok(mut f) = File::open(path) {
                        if f.read_to_end(&mut bytes).is_err() || bytes.is_empty() {
                            continue;
                        }
                    } else {
                        continue;
                    }

                    let file_name = path.file_name().unwrap_or_default().to_string_lossy().to_string();

                    let relative = match path.strip_prefix(source_path) {
                        Ok(rel) => rel,
                        Err(_) => continue,
                    };

                    let folder = if let Some(parent) = relative.parent() {
                        let parent_str = parent.to_string_lossy().replace('\\', "/");
                        if parent_str.is_empty() {
                            "INBOX".to_string()
                        } else {
                            clean_folder_path(&parent_str)
                        }
                    } else {
                        "INBOX".to_string()
                    };

                    map.entry(folder.clone()).or_default().push(RestoreItem {
                        folder,
                        eml_bytes: bytes,
                        file_name,
                    });
                }
            }
        }

        Ok(map)
    }

    /// Ejecuta el proceso completo de restauración / importación masiva hacia el buzón remoto
    pub fn process_restore(&self, source_path: &Path) -> RestoreSummary {
        let start_time = Instant::now();
        let mut summary = RestoreSummary {
            total_uploaded: 0,
            total_folders: 0,
            total_bytes: 0,
            total_errors: 0,
            elapsed_seconds: 0.0,
        };

        self.emit_log(LogEntry::info(format!(
            "Analizando contenido de respaldo en: {}",
            source_path.display()
        )));

        let folders_map = match Self::scan_backup_source(source_path) {
            Ok(map) => map,
            Err(e) => {
                let err_msg = format!("Error al analizar el origen de respaldo: {}", e);
                self.emit_log(LogEntry::error(&err_msg));
                summary.total_errors += 1;
                return summary;
            }
        };

        if folders_map.is_empty() {
            self.emit_log(LogEntry::warning(
                "No se encontraron mensajes .eml válidos en el origen seleccionado.",
            ));
            return summary;
        }

        let total_messages: usize = folders_map.values().map(|v| v.len()).sum();
        summary.total_folders = folders_map.len();

        self.emit_log(LogEntry::info(format!(
            "Se detectaron {} correos distribuidos en {} carpetas para subir a '{}'.",
            total_messages, summary.total_folders, self.email
        )));

        // 1. Conectar y obtener lista de carpetas y delimitador del servidor de destino
        self.emit_log(LogEntry::info(format!(
            "Conectando al servidor IMAP de destino {}:{}...",
            self.host, self.port
        )));

        let mut session = match self.connect_and_login() {
            Ok(s) => s,
            Err(e) => {
                let err_msg = format!("No se pudo conectar al servidor de destino: {}", e);
                self.emit_log(LogEntry::error(&err_msg));
                summary.total_errors += 1;
                return summary;
            }
        };

        let (existing_folders, delimiter) = match self.get_remote_folders(&mut session) {
            Ok(res) => res,
            Err(e) => {
                self.emit_log(LogEntry::warning(format!(
                    "Advertencia al listar carpetas existentes: {}. Se continuará.",
                    e
                )));
                (HashSet::new(), '/')
            }
        };

        let delim_str = delimiter.to_string();

        // 2. Recrear carpetas y subir mensajes
        let mut overall_uploaded = 0u64;

        for (raw_folder, items) in folders_map {
            // Adaptar nombre de carpeta al formato y delimitador del servidor de destino
            let remote_folder_name = adapt_folder_name(&raw_folder, &delim_str);

            self.emit_log(LogEntry::info(format!(
                "📂 Preparando carpeta remota '{}' ({} correos)...",
                remote_folder_name,
                items.len()
            )));

            // Recrear carpeta remota si no existe y no es INBOX
            if !is_inbox(&remote_folder_name)
                && !existing_folders.contains(&remote_folder_name.to_lowercase())
            {
                match session.create(&remote_folder_name) {
                    Ok(_) => {
                        self.emit_log(LogEntry::success(format!(
                            "✔ Carpeta remota '{}' creada exitosamente en el buzón.",
                            remote_folder_name
                        )));
                    }
                    Err(e) => {
                        // Puede que ya existiera o fuera una subcarpeta creada implícitamente
                        self.emit_log(LogEntry::info(format!(
                            "Aviso de creación para '{}': {} (continuando subida)",
                            remote_folder_name, e
                        )));
                    }
                }
            }

            // Subir mensajes por APPEND
            let mut folder_uploaded = 0u64;
            let folder_total = items.len() as u64;

            for item in items {
                let eml_len = item.eml_bytes.len() as u64;

                let mut upload_success = false;
                for attempt in 1..=self.retry_attempts {
                    // APPEND del mensaje RFC 822 íntegro
                    let append_res = session.append(&remote_folder_name, &item.eml_bytes).finish();

                    match append_res {
                        Ok(_) => {
                            upload_success = true;
                            break;
                        }
                        Err(e) => {
                            if attempt < self.retry_attempts {
                                self.emit_log(LogEntry::warning(format!(
                                    "Reintentando subida de '{}' en '{}' tras error: {}. Esperando {}s...",
                                    item.file_name, remote_folder_name, e, self.retry_delay_secs
                                )));
                                thread::sleep(Duration::from_secs(self.retry_delay_secs));

                                // Intentar reconectar si la sesión se cayó
                                if let Ok(new_session) = self.connect_and_login() {
                                    session = new_session;
                                }
                            } else {
                                self.emit_log(LogEntry::error(format!(
                                    "Error al subir mensaje '{}' a carpeta '{}': {}",
                                    item.file_name, remote_folder_name, e
                                )));
                            }
                        }
                    }
                }

                if upload_success {
                    folder_uploaded += 1;
                    overall_uploaded += 1;
                    summary.total_uploaded += 1;
                    summary.total_bytes += eml_len;
                } else {
                    summary.total_errors += 1;
                }

                self.emit_progress(
                    &remote_folder_name,
                    overall_uploaded,
                    total_messages as u64,
                    summary.total_bytes,
                );
            }

            self.emit_log(LogEntry::success(format!(
                "✔ Carpeta '{}' completada: {}/{} correos subidos.",
                remote_folder_name, folder_uploaded, folder_total
            )));
        }

        let _ = session.logout();

        summary.elapsed_seconds = start_time.elapsed().as_secs_f64();

        self.emit_log(LogEntry::success(format!(
            "=== RESTAURACIÓN MASIVA COMPLETADA EN {:.2}s: {} correos subidos ({:.2} MB), {} errores ===",
            summary.elapsed_seconds,
            summary.total_uploaded,
            summary.total_bytes as f64 / (1024.0 * 1024.0),
            summary.total_errors
        )));

        if let Some(ref tx) = self.event_sender {
            let _ = tx.send(RestoreEvent::Finished(summary.clone()));
        }

        summary
    }

    /// Obtiene las carpetas remotas existentes y el delimitador jerárquico
    fn get_remote_folders(
        &self,
        session: &mut imap::Session<native_tls::TlsStream<TcpStream>>,
    ) -> Result<(HashSet<String>, char)> {
        let mailboxes = session.list(None, Some("*"))?;
        let mut set = HashSet::new();
        let mut delim = '/';

        for mb in mailboxes.iter() {
            set.insert(mb.name().to_lowercase());
            if let Some(d) = mb.delimiter() {
                if let Some(c) = d.chars().next() {
                    delim = c;
                }
            }
        }

        Ok((set, delim))
    }
}

/// Limpia prefijos comunes de nombres de carpetas cuando el backup proviene de rutas como
/// `dominio/email/INBOX/SubFolder`
fn clean_folder_path(path_str: &str) -> String {
    let normalized = path_str.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();

    if parts.is_empty() {
        return "INBOX".to_string();
    }

    // Si los primeros segmentos parecen un dominio o un email (ej. "empresa.com", "usuario@empresa.com"), omitirlos
    let mut start_idx = 0;
    for (i, p) in parts.iter().enumerate() {
        if p.contains('@') || p.contains('.') && (p.ends_with(".com") || p.ends_with(".es") || p.ends_with(".net") || p.ends_with(".org")) {
            start_idx = i + 1;
        }
    }

    if start_idx < parts.len() {
        parts[start_idx..].join("/")
    } else {
        parts.join("/")
    }
}

/// Normaliza el nombre de la carpeta al delimitador del servidor de destino
fn adapt_folder_name(raw: &str, delim: &str) -> String {
    if is_inbox(raw) {
        return "INBOX".to_string();
    }

    let parts: Vec<&str> = raw.split(&['/', '\\', '.'][..]).filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        "INBOX".to_string()
    } else {
        parts.join(delim)
    }
}

fn is_inbox(folder: &str) -> bool {
    folder.eq_ignore_ascii_case("inbox") || folder.eq_ignore_ascii_case("bandeja de entrada")
}
