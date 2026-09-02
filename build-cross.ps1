# =====================================================================
# Script de Compilación y Cross-Compilation Automatizado (Rust)
# =====================================================================

param (
    [string]$Target = "windows", # "windows", "linux", o "all"
    [switch]$InstallRust = $false
)

Write-Host "=======================================================" -ForegroundColor Cyan
Write-Host "   COMPILADOR MULTIPLATAFORMA - IMAP BACKUP SUITE     " -ForegroundColor Green
Write-Host "=======================================================`n" -ForegroundColor Cyan

# 1. Comprobar instalación de Rust
$cargoInstalled = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargoInstalled) {
    Write-Host "[!] Rust/Cargo no está detectado en tu PATH." -ForegroundColor Yellow
    if ($InstallRust) {
        Write-Host "[*] Instalando Rust a través de winget..." -ForegroundColor Cyan
        winget install Rustlang.Rustup -e --accept-source-agreements --accept-package-agreements
        Write-Host "[+] Reinicia tu terminal tras la instalación de Rust para continuar." -ForegroundColor Green
        exit 0
    } else {
        Write-Host "    Para instalar Rust en Windows automáticamente ejecuta:" -ForegroundColor White
        Write-Host "    .\build-cross.ps1 -InstallRust`n" -ForegroundColor Cyan
        Write-Host "    O descarga el instalador oficial desde: https://rustup.rs/`n" -ForegroundColor White
        exit 1
    }
}

# 2. Compilar para Windows
if ($Target -eq "windows" -or $Target -eq "all") {
    Write-Host "[*] Compilando binario optimizado para Windows (x86_64-pc-windows-msvc)..." -ForegroundColor Cyan
    cargo build --release
    if ($LASTEXITCODE -eq 0) {
        Write-Host "[OK] Binario de Windows generado en: target\release\imap-backup-cli.exe`n" -ForegroundColor Green
    } else {
        Write-Host "[ERROR] Falló la compilación de Windows." -ForegroundColor Red
    }
}

# 3. Compilar para Linux (Cross-Compilation)
if ($Target -eq "linux" -or $Target -eq "all") {
    Write-Host "[*] Preparando compilación cruzada para Linux (x86_64-unknown-linux-gnu)..." -ForegroundColor Cyan
    
    # Comprobar si cross está instalado
    $crossInstalled = Get-Command cross -ErrorAction SilentlyContinue
    $dockerInstalled = Get-Command docker -ErrorAction SilentlyContinue

    if ($crossInstalled -and $dockerInstalled) {
        Write-Host "[*] Ejecutando 'cross' con entorno contenedorizado..." -ForegroundColor Cyan
        cross build --target x86_64-unknown-linux-gnu --release
        if ($LASTEXITCODE -eq 0) {
            Write-Host "[OK] Binario de Linux generado en: target\x86_64-unknown-linux-gnu\release\imap-backup-cli`n" -ForegroundColor Green
        }
    } else {
        Write-Host "[!] Para cross-compilar aplicaciones gráficas (GTK/X11) hacia Linux desde Windows se recomienda:" -ForegroundColor Yellow
        Write-Host "    1. Instalar Docker Desktop (https://www.docker.com/products/docker-desktop/)" -ForegroundColor White
        Write-Host "    2. Instalar cross: cargo install cross" -ForegroundColor White
        Write-Host "    3. Ejecutar: cross build --target x86_64-unknown-linux-gnu --release`n" -ForegroundColor Cyan
        Write-Host "    Alternativa Cloud (Cero instalación local):" -ForegroundColor White
        Write-Host "    Sube tu repositorio a GitHub y el flujo `.github/workflows/cross_compile.yml`" -ForegroundColor White
        Write-Host "    compilará automáticamente los binarios nativos de Windows y Linux en la nube.`n" -ForegroundColor Green
    }
}
