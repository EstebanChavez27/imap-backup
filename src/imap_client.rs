use crate::config::AccountConfig;
use crate::state::{BackupEvent, LogEntry};
use crate::storage::StorageManager;
use anyhow::{bail, Context, Result};
use native_tls::TlsConnector;
use std::collections::HashSet;
use std::net::TcpStream;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AccountStats {
    pub email: String,
    pub domain: String,
    pub total_folders: usize,
    pub total_messages: usize,
    pub downloaded_messages: usize,
    pub skipped_messages: usize,
    pub total_bytes: u64,
    pub errors: Vec<String>,
}

pub struct ImapDownloader<'a> {
    account: &'a AccountConfig,
    storage: &'a StorageManager,
    event_sender: Option<Sender<BackupEvent>>,
    retry_attempts: u32,
    retry_delay_secs: u64,
    timeout_secs: u64,
    skip_existing: bool,
}

impl<'a> ImapDownloader<'a> {
    pub fn new(
        account: &'a AccountConfig,
        storage: &'a StorageManager,
        event_sender: Option<Sender<BackupEvent>>,
        retry_attempts: u32,
        retry_delay_secs: u64,
        timeout_secs: u64,
        skip_existing: bool,
    ) -> Self {
        Self {
            account,
            storage,
            event_sender,
            retry_attempts,
            retry_delay_secs,
            timeout_secs,
            skip_existing,
        }
    }

    fn emit_log(&self, entry: LogEntry) {
        if let Some(ref tx) = self.event_sender {
            let _ = tx.send(BackupEvent::Log(entry));
        }
    }

    fn emit_progress(&self, folder: &str, current: u64, total: u64) {
        if let Some(ref tx) = self.event_sender {
            let _ = tx.send(BackupEvent::FolderProgress {
                account: self.account.email.clone(),
                folder: folder.to_string(),
                current,
                total,
            });
        }
    }

    /// Establece una conexión TLS con el servidor IMAP y realiza la autenticación
    fn connect_and_login(&self) -> Result<imap::Session<native_tls::TlsStream<TcpStream>>> {
        let tls_builder = TlsConnector::builder();
        let tls = tls_builder
            .build()
            .with_context(|| "Error inicializando conector TLS de native-tls")?;

        let tcp_stream = TcpStream::connect((self.account.host.as_str(), self.account.port))
            .with_context(|| {
                format!(
                    "No se pudo conectar vía TCP a {}:{}",
                    self.account.host, self.account.port
                )
            })?;

        let timeout = Duration::from_secs(self.timeout_secs);
        tcp_stream.set_read_timeout(Some(timeout))?;
        tcp_stream.set_write_timeout(Some(timeout))?;

        let tls_stream = tls
            .connect(&self.account.host, tcp_stream)
            .with_context(|| format!("Error en apretón de manos TLS con {}", self.account.host))?;

        let client = imap::Client::new(tls_stream);

        let session = client
            .login(&self.account.email, &self.account.password)
            .map_err(|(e, _)| anyhow::anyhow!("Fallo de autenticación IMAP: {}", e))?;

        Ok(session)
    }

    /// Ejecuta una operación con reintentos automáticos y registro de logs
    fn execute_with_retry<T, F>(&self, op_name: &str, mut op: F) -> Result<T>
    where
        F: FnMut() -> Result<T>,
    {
        let mut last_err = None;
        for attempt in 1..=self.retry_attempts {
            match op() {
                Ok(val) => return Ok(val),
                Err(e) => {
                    last_err = Some(e);
                    if attempt < self.retry_attempts {
                        let wait_time = self.retry_delay_secs * (attempt as u64);
                        self.emit_log(LogEntry::warning(format!(
                            "[{}] Reintento {}/{} para '{}': {}. Esperando {}s...",
                            self.account.email,
                            attempt,
                            self.retry_attempts,
                            op_name,
                            last_err.as_ref().unwrap(),
                            wait_time
                        )));
                        thread::sleep(Duration::from_secs(wait_time));
                    }
                }
            }
        }
        bail!(
            "Operación '{}' falló tras {} intentos. Error: {}",
            op_name,
            self.retry_attempts,
            last_err.unwrap()
        )
    }

    /// Procesa y descarga todas las carpetas y mensajes de la cuenta
    pub fn process_account(&self) -> AccountStats {
        let domain_label = self.account.get_domain_or_label();
        let mut stats = AccountStats {
            email: self.account.email.clone(),
            domain: domain_label.clone(),
            total_folders: 0,
            total_messages: 0,
            downloaded_messages: 0,
            skipped_messages: 0,
            total_bytes: 0,
            errors: Vec::new(),
        };

        if let Some(ref tx) = self.event_sender {
            let _ = tx.send(BackupEvent::AccountStarted {
                account: self.account.email.clone(),
            });
        }

        self.emit_log(LogEntry::info(format!(
            "[{}] Iniciando conexión con {}:{}...",
            self.account.email, self.account.host, self.account.port
        )));

        // Listar carpetas
        let folders_res = self.execute_with_retry("Listado de carpetas", || {
            let mut session = self.connect_and_login()?;
            let mailboxes = session
                .list(None, Some("*"))
                .with_context(|| "Error ejecutando comando LIST")?;

            let mut list = Vec::new();
            for mb in mailboxes.iter() {
                let name = mb.name().to_string();
                let delim = mb.delimiter().and_then(|d| d.chars().next());
                list.push((name, delim));
            }
            let _ = session.logout();
            Ok(list)
        });

        let folders = match folders_res {
            Ok(f) => f,
            Err(e) => {
                let err_msg = format!("Fallo al conectar/listar buzones: {}", e);
                stats.errors.push(err_msg.clone());
                self.emit_log(LogEntry::error(format!("[{}] {}", self.account.email, err_msg)));
                return stats;
            }
        };

        // Filtrar carpetas
        let target_folders: Vec<(String, Option<char>)> = folders
            .into_iter()
            .filter(|(f_name, _)| self.should_process_folder(f_name))
            .collect();

        stats.total_folders = target_folders.len();
        self.emit_log(LogEntry::info(format!(
            "[{}] {} carpetas encontradas para procesar.",
            self.account.email, stats.total_folders
        )));

        for (idx, (folder_name, delim)) in target_folders.iter().enumerate() {
            let folder_dir = self.storage.folder_path(
                &domain_label,
                &self.account.email,
                folder_name,
                *delim,
            );

            if let Err(e) = self.storage.ensure_dir(&folder_dir) {
                let err_msg = format!("Error creando carpeta en disco para {}: {}", folder_name, e);
                stats.errors.push(err_msg.clone());
                self.emit_log(LogEntry::error(format!("[{}] {}", self.account.email, err_msg)));
                continue;
            }

            self.emit_log(LogEntry::info(format!(
                "[{}] Procesando carpeta [{}/{}]: '{}'...",
                self.account.email,
                idx + 1,
                stats.total_folders,
                folder_name
            )));

            if let Err(e) = self.download_folder(folder_name, &folder_dir, &mut stats) {
                let err_msg = format!("Error en carpeta '{}': {}", folder_name, e);
                self.emit_log(LogEntry::error(format!("[{}] {}", self.account.email, err_msg)));
                stats.errors.push(err_msg);
            }
        }

        if stats.errors.is_empty() {
            self.emit_log(LogEntry::success(format!(
                "[{}] Completado con éxito ({} msgs nuevos, {} omitidos, {:.2} MB)",
                self.account.email,
                stats.downloaded_messages,
                stats.skipped_messages,
                stats.total_bytes as f64 / (1024.0 * 1024.0)
            )));
        } else {
            self.emit_log(LogEntry::warning(format!(
                "[{}] Finalizado con {} errores ({} msgs descargados)",
                self.account.email,
                stats.errors.len(),
                stats.downloaded_messages
            )));
        }

        stats
    }

    fn should_process_folder(&self, folder_name: &str) -> bool {
        let f_lower = folder_name.to_lowercase();

        for excluded in &self.account.exclude_folders {
            if f_lower == excluded.to_lowercase()
                || f_lower.ends_with(&format!("/{}", excluded.to_lowercase()))
                || f_lower.ends_with(&format!(".{}", excluded.to_lowercase()))
            {
                return false;
            }
        }

        if let Some(ref include_only) = self.account.include_only_folders {
            return include_only.iter().any(|inc| {
                f_lower == inc.to_lowercase()
                    || f_lower.ends_with(&format!("/{}", inc.to_lowercase()))
                    || f_lower.ends_with(&format!(".{}", inc.to_lowercase()))
            });
        }

        true
    }

    fn download_folder(
        &self,
        folder_name: &str,
        dest_dir: &Path,
        stats: &mut AccountStats,
    ) -> Result<()> {
        let uids: Vec<u32> = self.execute_with_retry("Obtener UIDs", || {
            let mut session = self.connect_and_login()?;
            session
                .examine(folder_name)
                .with_context(|| format!("No se pudo examinar carpeta {}", folder_name))?;

            let search_res = session
                .uid_search("ALL")
                .with_context(|| format!("Error en búsqueda de UIDs de {}", folder_name))?;

            let mut list: Vec<u32> = search_res.into_iter().collect();
            list.sort_unstable();
            let _ = session.logout();
            Ok(list)
        })?;

        if uids.is_empty() {
            self.emit_progress(folder_name, 0, 0);
            return Ok(());
        }

        stats.total_messages += uids.len();
        self.emit_progress(folder_name, 0, uids.len() as u64);

        let chunk_size = 25;
        let mut downloaded_uids = HashSet::new();
        let mut current_progress = 0u64;

        for chunk in uids.chunks(chunk_size) {
            let chunk_res = self.execute_with_retry("Descarga de lote de correos", || {
                let mut session = self.connect_and_login()?;
                session
                    .examine(folder_name)
                    .with_context(|| format!("Reabriendo carpeta {}", folder_name))?;

                let uid_range = chunk
                    .iter()
                    .map(|u| u.to_string())
                    .collect::<Vec<_>>()
                    .join(",");

                let fetches = session
                    .uid_fetch(&uid_range, "(BODY.PEEK[] UID INTERNALDATE FLAGS ENVELOPE)")
                    .with_context(|| format!("Error en FETCH de UIDs {}", uid_range))?;

                let mut fetched_messages = Vec::new();
                for msg in fetches.iter() {
                    let uid = msg.uid.unwrap_or(0);
                    let body_bytes = msg.body().or_else(|| msg.text()).map(|b| b.to_vec());
                    let message_id = msg
                        .envelope()
                        .and_then(|env| env.message_id.as_ref())
                        .map(|id| String::from_utf8_lossy(id).to_string());

                    fetched_messages.push((uid, message_id, body_bytes));
                }

                let _ = session.logout();
                Ok(fetched_messages)
            });

            match chunk_res {
                Ok(messages) => {
                    for (uid, message_id, body_bytes) in messages {
                        if uid == 0 || downloaded_uids.contains(&uid) {
                            continue;
                        }

                        let file_name = StorageManager::eml_filename(uid, message_id.as_deref());
                        let file_path = dest_dir.join(&file_name);

                        if let Some(body) = body_bytes {
                            let body_len = body.len() as u64;

                            if self.skip_existing
                                && StorageManager::file_exists_and_valid(&file_path, 1)
                            {
                                stats.skipped_messages += 1;
                            } else {
                                StorageManager::save_eml(&file_path, &body)?;
                                stats.downloaded_messages += 1;
                                stats.total_bytes += body_len;
                            }

                            downloaded_uids.insert(uid);
                            current_progress += 1;
                            self.emit_progress(folder_name, current_progress, uids.len() as u64);
                        } else {
                            stats.errors.push(format!(
                                "Mensaje UID {} en carpeta '{}' sin cuerpo RFC822",
                                uid, folder_name
                            ));
                            current_progress += 1;
                            self.emit_progress(folder_name, current_progress, uids.len() as u64);
                        }
                    }
                }
                Err(e) => {
                    stats.errors.push(format!(
                        "Error en lote de carpeta '{}': {}",
                        folder_name, e
                    ));
                    current_progress += chunk.len() as u64;
                    self.emit_progress(folder_name, current_progress, uids.len() as u64);
                }
            }
        }

        Ok(())
    }
}
