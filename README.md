# 🚀 IMAP Email Backup & Migration Suite (GUI Nativa en Rust)

Aplicación de escritorio **100% nativa en Rust**, ultra ligera y de alto rendimiento, diseñada con **`egui`** y **`eframe`** para respaldar, sincronizar y migrar buzones de correo electrónico vía **IMAP sobre TLS**.

Empaqueta los correos en formato estándar `.eml` (RFC 822) organizados por dominio y cuenta, y genera archivos `.zip` estructurados listos para migraciones entre proveedores de hosting (Hostinger, cPanel, Plesk, Google Workspace, Microsoft 365, etc.).

---

## ⚡ ¿Por qué esta arquitectura?

- **Cero WebViews (No Electron / No Tauri):** Renderizado directo por hardware (GPU/OpenGL) en modo inmediato.
- **Consumo Mínimo de Recursos:** La aplicación consume menos de **35 MB de RAM** y genera un único binario ejecutable portable.
- **Interfaz Fluida y No Bloqueante:** Toda la descarga de red y compresión corre en un hilo de trabajo asíncrono con **`tokio`**, comunicándose con la GUI a través de canales de mensajes (`std::sync::mpsc`), manteniendo la ventana respondiendo a 60 FPS sin congelarse.
- **Diálogos de Archivos Nativos (`rfd`):** Utiliza los selectores oficiales de archivos de Windows y Linux.

---

## 🖥️ Características de la Interfaz Gráfica

1. **Gestión de Configuración y Cuentas:**
   - Cargar y guardar archivos de configuración en formato **TOML** (`config.toml`) o **JSON** (`config.json`).
   - Tabla interactiva para visualizar las cuentas configuradas.
   - Diálogo modal para **Añadir**, **Editar** y **Eliminar** credenciales directamente desde la UI.
2. **Control de Opciones Globales:**
   - Selector visual de carpeta de destino.
   - Ajuste dinámico del límite de concurrencia (1 a 10 cuentas simultáneas).
   - Selector de modo ZIP (`Por Cuenta`, `Consolidado Maestro`, `Sin Compresión`).
   - Interruptores para limpieza de `.eml` temporales y sincronización incremental.
3. **Monitoreo en Tiempo Real:**
   - Barra de progreso por cada buzón y carpeta IMAP.
   - Consola de actividad integrada con colores por tipo de evento (`INFO`, `WARN`, `ERROR`, `OK`) y auto-scroll.
   - Notificaciones emergentes de estado.

---

## 📁 Estructura del Código

```
exportador_de_correos/
├── Cargo.toml                  # Dependencias de egui, eframe, rfd, tokio, imap, zip
├── config.example.toml         # Plantilla TOML de ejemplo
├── config.example.json         # Plantilla JSON de ejemplo
├── README.md                   # Esta documentación
└── src/
    ├── main.rs                 # Inicialización de eframe y ventana de escritorio
    ├── app.rs                  # Componente principal de la GUI (eframe::App) y dispatch asíncrono
    ├── state.rs                # Modelos de eventos MPSC, logs y estados de la UI
    ├── config.rs               # Deserialización y validación (JSON / TOML)
    ├── imap_client.rs          # Cliente IMAP (TLS, paginación, descarga RFC 822 y reintentos)
    ├── storage.rs              # Manejo seguro de directorios y nombres de archivos .eml
    └── archiver.rs             # Compresor ZIP recursivo y limpieza
```

---

## 🛠️ Requisitos de Sistema y Compilación

### 🪟 En Windows

1. Instalar [Rust y Cargo](https://rustup.rs/) (herramienta `x86_64-pc-windows-msvc` recomendada con Visual Studio Build Tools).
2. Compilar el ejecutable optimizado:
   ```powershell
   cargo build --release
   ```
3. El binario ejecutable se genera en:
   ```
   target\release\imap-backup-cli.exe
   ```
   *(Incluye `#![windows_subsystem = "windows"]` en modo release para no abrir una ventana negra de consola CMD).*

---

### 🐧 En Linux (Ubuntu / Debian / Linux Mint)

Para compilar interfaces `egui` y selectores nativos `rfd` en Linux se requieren las librerías de desarrollo de GTK y X11:

```bash
# Instalar paquetes requeridos
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev libgtk-3-dev \
                    libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
```

Compilar para Linux:
```bash
cargo build --release
```
El ejecutable binario se generará en:
```
target/release/imap-backup-cli
```

---

### 🌐 Compilación Cruzada (Cross-Compilation)

Si deseas compilar para Linux desde Windows o viceversa:

#### Opción A: Usando la herramienta estándar `cross`
```bash
# Instalar cross (requiere Docker o Podman)
cargo install cross

# Generar binario para Linux de 64 bits:
cross build --target x86_64-unknown-linux-gnu --release

# Generar binario para Windows desde Linux:
cross build --target x86_64-pc-windows-gnu --release
```

---

## 🔄 Integración Asíncrona (Cómo no bloquear la UI)

El flujo de comunicación entre el hilo de la UI y el motor de descarga funciona de la siguiente manera:

```
+-------------------------------------------------------------+
|                     Hilo Principal (GUI)                    |
|                                                             |
|  - Renderiza con eframe a 60 FPS                            |
|  - En cada frame ejecuta `process_events()`                 |
|  - Consume mensajes no bloqueantes con `rx.try_recv()`      |
|  - Si hay descargas activas, llama `ctx.request_repaint()`  |
+------------------------------▲------------------------------+
                               |
                   Canal `std::sync::mpsc`
             (BackupEvent: Log, Progress, Finished)
                               |
+------------------------------┴------------------------------+
|             Hilo de Fondo (Tokio Async Runtime)             |
|                                                             |
|  - Semáforo de concurrencia (`tokio::sync::Semaphore`)      |
|  - Clientes IMAP paralelos en `spawn_blocking`              |
|  - Emisión de logs y progreso en tiempo real                |
|  - Compresión ZIP final                                     |
+-------------------------------------------------------------+
```

---

## 📄 Licencia

Este proyecto está licenciado bajo los términos de la **GNU Affero General Public License v3.0 (AGPL-3.0-or-later)**. Consulta el archivo [`LICENSE`](LICENSE) para ver el texto completo de la licencia.

### ⚖️ Derechos y Obligaciones Principales

- **Libertad de Uso y Modificación:** Puedes usar, estudiar, modificar y compilar este software libremente para fines personales, empresariales o comerciales.
- **Copyleft Fuerte:** Si modificas este programa o creas obras derivadas y las distribuyes (ya sea en código fuente o en binarios ejecutables), debes publicar el código fuente completo bajo los mismos términos de la GNU AGPLv3.
- **Cláusula de Interacción en Red (Sección 13):** A diferencia de la GPL estándar, si ejecutas una versión modificada de esta herramienta en un servidor remoto o como servicio en la nube (SaaS / Web Service) para permitir que otros interactúen con ella a través de una red, estás legalmente obligado a poner el código fuente correspondiente a disposición de todos los usuarios de la red de forma pública y gratuita.
- **Sin Garantía:** El software se distribuye "tal cual" (*as-is*), sin garantías expresas o implícitas de ningún tipo.

