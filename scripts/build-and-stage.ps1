param(
    [switch]$SkipBuild = $false
)

$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

if (-not $SkipBuild) {
    Write-Host "[1/3] Frontend derleniyor..."
    .\node_modules\.bin\tsc.cmd --noEmit
    .\node_modules\.bin\vite.cmd build

    Set-Location "$RepoRoot\src-tauri"
    Write-Host "[2/3] ozan-apo.dll (User-Mode APO COM DLL) derleniyor..."
    cargo build --release --offline -p ozan-apo
    Copy-Item -Force "target\release\ozan_apo.dll" "target\release\ozan-apo.dll"

    Write-Host "[3/3] ozan.exe derleniyor..."
    cargo build --release --offline --bin ozan
    Set-Location $RepoRoot
}

$OutDir = Join-Path $RepoRoot "dist-artifacts"
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$ExeSrc = Join-Path $RepoRoot "src-tauri\target\release\ozan.exe"
$DllSrc = Join-Path $RepoRoot "src-tauri\target\release\ozan-apo.dll"
if (-not (Test-Path $DllSrc)) {
    $DllSrc = Join-Path $RepoRoot "src-tauri\target\release\ozan_apo.dll"
}

Copy-Item -Force $ExeSrc (Join-Path $OutDir "ozan.exe")
Copy-Item -Force $DllSrc (Join-Path $OutDir "ozan-apo.dll")

$ExeHash = (Get-FileHash -Algorithm SHA256 (Join-Path $OutDir "ozan.exe")).Hash.ToLower()
$DllHash = (Get-FileHash -Algorithm SHA256 (Join-Path $OutDir "ozan-apo.dll")).Hash.ToLower()

$Sums = @(
    "$ExeHash  ozan.exe",
    "$DllHash  ozan-apo.dll"
) -join "`n"

Set-Content -Path (Join-Path $OutDir "SHA256SUMS.txt") -Value $Sums -Encoding UTF8
Write-Host "SignPath.io uyumlu artefaktlar hazirlandi: $OutDir"
Write-Host $Sums
