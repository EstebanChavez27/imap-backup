# imap-backup 2.0 (`v2/`)

Reescritura del respaldo de correo IMAP con **sincronización incremental real**,
verificación local, restauración idempotente, migración directa servidor→servidor y
**modo headless** (CLI) además de la interfaz gráfica.

La versión 1 sigue intacta en la raíz del repositorio (`src/`, `Cargo.toml`): esta
carpeta es un proyecto aparte, con su propio `Cargo.toml`, y no la modifica.

```bash
cd v2
cargo build --release          # binario: target/release/imap-backup(.exe)
cargo run -- help              # ayuda completa del modo headless
cargo run --bin imap-backup-gui  # interfaz gráfica
cargo test                     # 110 pruebas unitarias
cargo clippy --all-targets     # sin advertencias
```

Sin interfaz gráfica (binario mucho más chico, sin GTK/X11):

```bash
cargo build --release --no-default-features
```

---

## Qué se corrigió respecto de la v1

| Problema en la v1 | Cómo queda en la v2 |
| --- | --- |
| Corte por bytes del nombre del `.eml` → `panic` en medio de un backup (y con `panic = "abort"` moría la aplicación) | Corte por caracteres (`truncate_chars`) + pruebas con `Message-ID` multibyte. El perfil release ya no usa `panic = "abort"`: un hilo que falla no aborta el respaldo |
| `tls` se ignoraba: siempre TLS directo | `ConnectionMode` según la cuenta: **TLS directo, STARTTLS o sin cifrado** (`imap` 3.0). También en restauración y en el destino de la migración |
| Se pedían `INTERNALDATE` y `FLAGS` pero se descartaban; todo correo restaurado quedaba sin leer y con la fecha de hoy | Fechas y flags se guardan en el manifiesto y **se suben** al restaurar/migrar (`APPEND` con `.flag()` e `.internal_date()`) |
| La "sincronización incremental" bajaba todos los cuerpos para después descartarlos | Planificador puro: primero metadatos (`RFC822.SIZE`/UID `FETCH` sin cuerpo), se descargan **solo los cuerpos que faltan o cambiaron**. `UIDVALIDITY`/`UIDNEXT` del manifiesto evitan incluso pedir metadatos cuando no hay novedades |
| `file_exists_and_valid` daba por bueno cualquier archivo de ≥1 byte; los `.eml` truncados quedaban "válidos" para siempre | Escritura atómica (archivo temporal + `rename`) y comprobación de tamaño y SHA-256 opcional |
| Se ignoraba `UIDVALIDITY` | Se guarda por carpeta: si el buzón se recrea, el plan fuerza la resincronización en vez de dar por buenos archivos viejos |
| Escrituras y ZIP sin progreso ni cancelación | Progreso por bytes y por mensaje, **cancelación en caliente**, presupuesto de memoria por lote (`chunk_bytes`, `max_chunk_messages`) |
| Todo en un hilo; errores con `String` | Trabajadores en paralelo por cuenta y por carpeta, reintentos con backoff exponencial, `anyhow` con contexto y reporte por carpeta |

Además se agregó lo que la v1 no tenía: manifiesto por cuenta, verificación local,
restauración idempotente, migración directa, formato mbox/Maildir, ZIP con progreso,
reportes JSON/CSV, hooks, modo headless con códigos de salida y log a archivo.

---

## Cómo funciona el incremental

Por cada carpeta se guarda en `_imap-backup-manifest.json`:

- `uid_validity` y `uid_next` del servidor;
- por mensaje: `uid`, `message_id`, `size`, `internal_date`, `flags`, ruta del archivo
  y `sha256` cuando `hash_verify = true`.

En cada corrida:

1. `LIST` + `EXAMINE` para conocer carpetas, `UIDVALIDITY` y `UIDNEXT`.
2. `plan_metadata_fetch`: si `UIDVALIDITY` cambió → se reexamina todo; si `UIDNEXT` no
   cambió → no se pide nada; si no → solo el rango nuevo (`uid_next:*`).
3. `plan_sync`: se compara cada mensaje del servidor con el manifiesto y se decide
   *bajar*, *saltar* o *rehacer* (archivo ausente, tamaño distinto o hash roto).
4. Descarga por lotes acotados en bytes, con escritura atómica y manifiesto
   actualizado por carpeta (reanudable: si se corta, la próxima corrida continúa).

El planificador (`manifest::plan_sync`) es una función pura y está cubierto por
pruebas: no toca red ni disco, así que su comportamiento se puede verificar sin un
servidor.

## Estructura en disco

```
<salida>/<etiqueta o dominio>/<email>/
    _imap-backup-manifest.json      # índice, UIDVALIDITY, flags, fechas, hashes
    INBOX/1_<message-id>.eml        # export_format = "eml" (por defecto)
    Enviados.mbox                   # export_format = "mbox"
    Archivo/cur/12_<id>:2,S         # export_format = "maildir" (";"  en Windows)
```

- Los nombres remotos originales siempre están en el manifiesto: la carpeta en disco
  se sanitiza (rutas largas, caracteres inválidos en Windows) pero nunca se pierde el
  nombre real ni el separador del servidor de origen.
- `zip_mode = "per-account"` (un ZIP por cuenta), `"consolidated"` (un ZIP maestro) o
  `"none"`. El ZIP nunca se incluye a sí mismo ni deja archivos temporales dentro.
- Los mbox se normalizan a LF al escribir y a CRLF al leer; el escape mboxrd (líneas
  que empiezan con `From `) es reversible.

## Restauración

Acepta un ZIP, una carpeta de `.eml`, un `.mbox`, un Maildir **o un ZIP que contenga
cualquiera de esos formatos** (los mbox dentro del ZIP se indexan y cada mensaje se
descomprime bajo demanda). El análisis del origen es un paso aparte con progreso: en
la GUI la carpeta se indexa en segundo plano.

- **Idempotente**: deduplica por `Message-ID` contra el destino (`dedup_on_restore`),
  así que repetir una restauración no duplica correo.
- Preserva fechas y flags (`\Seen`, `\Flagged`, …) e informa cuántos mensajes subió,
  omitió y falló.
- `--prefix NOMBRE` restaura dentro de una carpeta contenedora (útil para revisar una
  migración antes de fusionarla).
- `--dry-run` muestra qué se haría sin escribir nada.
- Restaura en paralelo por carpeta; el `APPEND` se hace en streaming, sin guardar el
  mensaje completo en memoria.

## Migración directa servidor→servidor

Lee del origen y escribe en el destino por streaming (sin ZIP ni temporales), con las
mismas garantías: deduplicación por `Message-ID` en el destino, fechas/flags
preservados, reanudable, con `--keep-local DIR` si además querés una copia local.
Sirve para dejar atrás un proveedor sin pasar por un cliente de correo completo.

## Verificación local

```bash
cargo run -- verify --output ./backup      # no necesita configuración
```

Recorre el manifiesto y comprueba que cada mensaje siga en disco, con el tamaño
esperado y el mismo SHA-256 cuando se registró. **Sin configuración**: si no hay
cuentas, descubre las carpetas con manifiesto dentro de `--output`. Es la respuesta a
"¿puedo confiar en este backup?" sin tocar el servidor.

## Línea de comandos (headless)

```
backup | restore | migrate | test-connection | verify | extract | gui | help | version
```

Pensado para cron / Programador de tareas: `--quiet`, `--log-file`, `--report` (JSON +
CSV), `--json` a la salida estándar y códigos de salida utilizables por scripts:

| Código | Significado |
| --- | --- |
| 0 | terminó bien |
| 1 | terminó con errores (mirar el reporte o el log) |
| 2 | error de uso o de configuración |
| 3 | error inesperado en ejecución |
| 130 | cancelado por el usuario (Ctrl+C) |

Detalles pensados para automatizar:

- Si la entrada estándar no es una terminal, **no se pide contraseña**: falla con un
  mensaje que indica usar `password_env`, el archivo de secretos o `--password-env`.
  Un cron nunca queda colgado esperando algo que no va a llegar.
- `--password-env VAR` evita dejar contraseñas en el archivo de configuración.
- `--force-full` ignora el manifiesto y vuelve a bajar todo (reconstrucción completa).

## Configuración

Compatible con los archivos de la v1: se leen TOML o JSON, el `tls = true/false` de la
v1 se traduce a TLS directo / sin cifrado y los alias antiguos siguen funcionando
(`retry_backoff_base_secs`, `timeout`). El orden de búsqueda es: ruta explícita
(`--config`), directorio actual, directorio del ejecutable (modo portable) y
directorio de configuración del usuario.

Ajustes nuevos: `verify_after_backup`, `hash_verify`, `dedup_on_restore`,
`export_format`, `keep_eml_alongside`, `skip_all_mail`, `chunk_bytes`,
`max_chunk_messages`, `restore_concurrency`, `retry_attempts`,
`retry_backoff_base_secs`, `retry_backoff_max_secs`, `timeout_secs` y `hooks`
(`post_backup_command`, `post_restore_command`, `failure_webhook_url`,
`webhook_on_success`).

Autenticación: contraseña en el archivo, `password_env`, archivo de secretos aparte
(la GUI guarda la configuración **sin** contraseñas) u OAuth2 vía `oauth_token_env`
para Microsoft 365 y Google Workspace, donde la autenticación básica ya no existe.

## Interfaz gráfica

Tres pestañas (Respaldo, Restaurar, Migrar) con progreso por cuenta, **botón de
cancelar** que realmente interrumpe, log virtualizado con miles de líneas, "Probar
conexión" por cuenta, análisis del origen en segundo plano y guardado de configuración
sin secretos. Errores de TLS, credenciales y espacio libre en disco se muestran antes
de empezar, no a mitad del respaldo.

## Seguridad

- Las credenciales nunca se escriben en el log ni en el reporte; el archivo de
  secretos se crea con permisos restringidos (0600 en Unix).
- `insecure_skip_tls_verify` está desactivado por defecto y la GUI avisa cuando se
  activa.
- Los valores que llegan desde configuración se limpian de CR/LF antes de usarse en
  comandos IMAP (defensa contra inyección).
- Los nombres de archivo se sanitizan contra traversal (`..`, separadores, nombres
  reservados de Windows).

## Pruebas

110 pruebas unitarias, sin red: planificador de sincronización, manifiesto,
escritura atómica, mbox (round-trip, escape, fechas mal formadas), Maildir, ZIP,
configuración (v1 → v2, secretos, avisos), parser de la CLI, códigos de salida,
webhook HTTP (incluido `Transfer-Encoding: chunked`), reportes y verificación.

## Pendiente / ideas siguientes

- Restauración incremental guiada por `Message-ID` del origen (hoy decide el destino).
- Catálogo con búsqueda de mensajes (`buscar en todos los backups sin abrir la GUI`).
- Notificación por webhook con resumen del archivo en lugar de solo fallos.
- Empaquetado (instalador en Windows, `cargo-deb`/AUR en Linux) y firma de binarios.
- Exportar a formatos de archivo (`.pst`/`.tgz` particionado) para entrega a terceros.
