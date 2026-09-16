// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Hooks posteriores a la corrida: comando del sistema y webhook HTTP(S).
//!
//! El webhook se implementa con `native-tls` y `TcpStream` (sin traer un cliente HTTP
//! completo) porque solo necesita un POST con cuerpo JSON y leer el código de estado.

use crate::events::Ctx;
use anyhow::{bail, Context, Result};
use native_tls::TlsConnector;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

/// Ejecuta un comando del shell con el resumen en variables de entorno
/// (`IMAPB_*`) para que el script decida qué hacer (copiar a un NAS, avisar, etc.).
pub fn run_shell_hook(ctx: &Ctx, command: &str, env: &[(&str, String)]) -> Result<()> {
    let command = command.trim();
    if command.is_empty() {
        return Ok(());
    }

    #[cfg(windows)]
    let mut process = {
        let mut cmd = std::process::Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    };
    #[cfg(not(windows))]
    let mut process = {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    };

    for (key, value) in env {
        process.env(key, value);
    }

    ctx.info("hook", format!("Ejecutando: {}", command));
    match process.output() {
        Ok(output) if output.status.success() => {
            ctx.success("hook", format!("Comando finalizado con {}", output.status));
            Ok(())
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.trim();
            ctx.warn(
                "hook",
                format!(
                    "El comando terminó con {}{}",
                    output.status,
                    if detail.is_empty() { String::new() } else { format!(": {}", detail) }
                ),
            );
            Ok(())
        }
        Err(err) => {
            ctx.warn("hook", format!("No se pudo ejecutar el comando: {}", err));
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUrl {
    pub https: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Interpreta una URL `http(s)://host[:puerto]/ruta`.
pub fn parse_url(url: &str) -> Result<ParsedUrl> {
    let trimmed = url.trim();
    let (https, rest) = if let Some(rest) = trimmed.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        (false, rest)
    } else {
        bail!("La URL debe empezar con http:// o https://");
    };

    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        bail!("La URL no incluye host");
    }

    let (host, port) = if let Some((host, port_text)) = authority.rsplit_once(':') {
        if host.contains('/') || host.is_empty() {
            (authority, None)
        } else {
            let port = port_text
                .parse::<u16>()
                .with_context(|| format!("Puerto inválido en la URL: {}", port_text))?;
            (host, Some(port))
        }
    } else {
        (authority, None)
    };

    Ok(ParsedUrl {
        https,
        host: host.to_string(),
        port: port.unwrap_or(if https { 443 } else { 80 }),
        path: if path.is_empty() { "/".to_string() } else { path.to_string() },
    })
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// POST con cuerpo JSON. Devuelve el código de estado y el cuerpo de la respuesta.
pub fn http_post_json(url: &str, body: &str, timeout: Duration) -> Result<HttpResponse> {
    let parsed = parse_url(url)?;

    let tcp = TcpStream::connect((parsed.host.as_str(), parsed.port))
        .with_context(|| format!("No se pudo conectar a {}:{}", parsed.host, parsed.port))?;
    tcp.set_read_timeout(Some(timeout)).ok();
    tcp.set_write_timeout(Some(timeout)).ok();

    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: imap-backup/2.0\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        parsed.path,
        parsed.host,
        body.len(),
        body
    );

    let raw = if parsed.https {
        let connector = TlsConnector::builder()
            .build()
            .context("Error inicializando TLS para el webhook")?;
        let mut stream = connector
            .connect(&parsed.host, tcp)
            .with_context(|| format!("Error TLS al conectar con {}", parsed.host))?;
        stream.write_all(request.as_bytes())?;
        stream.flush()?;
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .context("Error leyendo la respuesta del webhook")?;
        response
    } else {
        let mut stream = tcp;
        stream.write_all(request.as_bytes())?;
        stream.flush()?;
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .context("Error leyendo la respuesta del webhook")?;
        response
    };

    if raw.is_empty() {
        bail!("El webhook no devolvió respuesta");
    }

    let status = parse_status_line(&raw)?;
    let (_, raw_body) = split_headers(&raw);
    let body_text = if raw_body.is_empty() {
        String::new()
    } else if is_chunked(&raw) {
        dechunk(raw_body)
    } else {
        String::from_utf8_lossy(raw_body).to_string()
    };

    Ok(HttpResponse {
        status,
        body: body_text,
    })
}

fn parse_status_line(raw: &[u8]) -> Result<u16> {
    let text = String::from_utf8_lossy(raw);
    let line = text.lines().next().unwrap_or_default();
    let mut parts = line.split_whitespace();
    let _http = parts.next();
    let status = parts
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .with_context(|| format!("Respuesta HTTP ilegible: {}", line))?;
    Ok(status)
}

fn split_headers(raw: &[u8]) -> (String, &[u8]) {
    match find_subslice(raw, b"\r\n\r\n") {
        Some(index) => (
            String::from_utf8_lossy(&raw[..index]).to_string(),
            &raw[index + 4..],
        ),
        None => (String::from_utf8_lossy(raw).to_string(), &[]),
    }
}

fn is_chunked(raw: &[u8]) -> bool {
    let (headers, _) = split_headers(raw);
    headers
        .to_lowercase()
        .contains("transfer-encoding: chunked")
}

/// Decodifica `Transfer-Encoding: chunked` (algunos servicios lo usan incluso con
/// `Connection: close`).
fn dechunk(body: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::new();
    let mut cursor = 0usize;
    while let Some(offset) = find_subslice(&body[cursor..], b"\r\n") {
        let line_end = cursor + offset;
        let size_text = String::from_utf8_lossy(&body[cursor..line_end]);
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = match usize::from_str_radix(size_text, 16) {
            Ok(size) => size,
            Err(_) => break,
        };
        if size == 0 {
            break;
        }
        let start = line_end + 2;
        let end = (start + size).min(body.len());
        out.extend_from_slice(&body[start..end]);
        cursor = end + 2;
        if cursor >= body.len() {
            break;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Envía el resumen al webhook configurado. Nunca devuelve error fatal: un webhook
/// caído no debe invalidar un backup correcto.
pub fn notify_webhook(ctx: &Ctx, url: &str, payload_json: &str) {
    match http_post_json(url, payload_json, Duration::from_secs(15)) {
        Ok(response) if (200..300).contains(&response.status) => {
            ctx.info("webhook", format!("Notificación enviada (HTTP {})", response.status));
        }
        Ok(response) => {
            ctx.warn(
                "webhook",
                format!(
                    "El webhook respondió HTTP {}: {}",
                    response.status,
                    crate::folders::truncate_chars(response.body.trim(), 200)
                ),
            );
        }
        Err(err) => {
            ctx.warn("webhook", format!("No se pudo enviar el webhook: {}", err));
        }
    }
}

pub fn report_path_hint(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls_with_and_without_port_and_path() {
        let url = parse_url("https://hooks.example.com/services/abc").unwrap();
        assert!(url.https);
        assert_eq!(url.host, "hooks.example.com");
        assert_eq!(url.port, 443);
        assert_eq!(url.path, "/services/abc");

        let url = parse_url("http://localhost:8080/hook").unwrap();
        assert!(!url.https);
        assert_eq!(url.host, "localhost");
        assert_eq!(url.port, 8080);
        assert_eq!(url.path, "/hook");

        let url = parse_url("https://example.com").unwrap();
        assert_eq!(url.path, "/");
        assert!(parse_url("ftp://example.com").is_err());
        assert!(parse_url("https://host:no-numero/x").is_err());
    }

    #[test]
    fn dechunks_bodies() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nhola\r\n6\r\n mundo\r\n0\r\n\r\n";
        assert_eq!(parse_status_line(raw).unwrap(), 200);
        assert!(is_chunked(raw));
        let (_, body) = split_headers(raw);
        assert_eq!(dechunk(body), "hola mundo");
    }

    #[test]
    fn reads_status_and_plain_body() {
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\n\r\nya!";
        assert_eq!(parse_status_line(raw).unwrap(), 404);
        assert!(!is_chunked(raw));
        let (headers, body) = split_headers(raw);
        assert!(headers.contains("Content-Length"));
        assert_eq!(String::from_utf8_lossy(body), "ya!");
    }
}
