#requires -Version 5
# Ozan - geliştirme ortamı kurulum betiği (Rust, MSVC Build Tools, Node.js, Tauri CLI)
$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$dl = Join-Path $env:TEMP 'ozan-setup'
New-Item -ItemType Directory -Force -Path $dl | Out-Null

function Add-UserPath([string]$p) {
    $cur = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not ($cur -split ';' | Where-Object { $_ -ieq $p })) {
        [Environment]::SetEnvironmentVariable('Path', ($cur.TrimEnd(';') + ';' + $p), 'User')
    }
}

# ---------- Node.js (portable, yönetici gerektirmez) ----------
$nodeDir = Join-Path $env:LOCALAPPDATA 'Programs\nodejs'
if (-not (Test-Path (Join-Path $nodeDir 'node.exe'))) {
    Write-Host '[node] sürüm bilgisi alınıyor...'
    $idx = Invoke-RestMethod 'https://nodejs.org/dist/index.json'
    $lts = $idx | Where-Object { $_.lts -and $_.files -contains 'win-x64-zip' } | Select-Object -First 1
    $ver = $lts.version
    $zip = Join-Path $dl "node-$ver-win-x64.zip"
    Write-Host "[node] $ver indiriliyor..."
    Invoke-WebRequest "https://nodejs.org/dist/$ver/node-$ver-win-x64.zip" -OutFile $zip -UseBasicParsing
    Expand-Archive $zip -DestinationPath $dl -Force
    New-Item -ItemType Directory -Force -Path (Split-Path $nodeDir) | Out-Null
    if (Test-Path $nodeDir) { Remove-Item $nodeDir -Recurse -Force }
    Move-Item (Join-Path $dl "node-$ver-win-x64") $nodeDir
    Add-UserPath $nodeDir
    Write-Host "[node] kuruldu: $nodeDir"
} else { Write-Host '[node] zaten kurulu' }

# ---------- Rust (rustup, yönetici gerektirmez) ----------
$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
if (-not (Test-Path $cargo)) {
    $ri = Join-Path $dl 'rustup-init.exe'
    Write-Host '[rust] rustup-init indiriliyor...'
    Invoke-WebRequest 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe' -OutFile $ri -UseBasicParsing
    & $ri -y --default-toolchain stable --profile minimal --default-host x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw "rustup-init başarısız: $LASTEXITCODE" }
    Write-Host '[rust] kuruldu'
} else { Write-Host '[rust] zaten kurulu' }

# ---------- MSVC C++ Build Tools & Windows SDK (portable, yönetici gerektirmez) ----------
$msvcDir = Join-Path $env:LOCALAPPDATA 'Programs\msvc'
if (-not (Test-Path (Join-Path $msvcDir 'setup_x64.bat'))) {
    $pmsvc = Join-Path $dl 'portable-msvc.py'
    if (-not (Test-Path $pmsvc)) {
        Invoke-WebRequest 'https://gist.githubusercontent.com/mmozeiko/7f3162ec2988e81e56d5c4e22cde9977/raw/portable-msvc.py' -OutFile $pmsvc -UseBasicParsing
    }
    Push-Location (Join-Path $env:LOCALAPPDATA 'Programs')
    try {
        python -u $pmsvc --accept-license --vs 2022 --target x64 --host x64
    } finally {
        Pop-Location
    }
} else { Write-Host '[msvc] zaten kurulu' }

Write-Host 'TAMAM'
