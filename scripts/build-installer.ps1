# Builds Eudora's Windows installer: dist\Eudora-Setup-<version>.exe (Inno Setup 6, as
# PlanetPDF's).
#
#   .\scripts\build-installer.ps1            # build the programs, then the installer
#   .\scripts\build-installer.ps1 -NoBuild   # the installer from what's built already
#   .\scripts\build-installer.ps1 -Arch arm64  # Windows on ARM: ...-Setup-<version>-arm64.exe
#       (needs rustup's aarch64-pc-windows-msvc target, the MSVC ARM64 build tools, and LLVM's
#       clang, which the ring library builds its ARM64 code with)
#
# Steps (counted): 1 the programs (cargo, release), 2 the staging folder, 3 the installer.
# Needs Inno Setup 6 (winget install JRSoftware.InnoSetup). The version is the workspace's
# (Cargo.toml [workspace.package] version).
param([switch]$NoBuild, [ValidateSet('x64', 'arm64')][string]$Arch = 'x64')
$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot
$iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
          "$env:ProgramFiles\Inno Setup 6\ISCC.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 isn't installed (winget install JRSoftware.InnoSetup)." }

$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $version) { throw 'No version in Cargo.toml' }

$target = if ($Arch -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { $null }
Write-Output "Step 1 of 3: building Eudora $version for $Arch (eudora, eudora-cli, eudora-web)"
if (-not $NoBuild) {
    Push-Location $root
    try {
        # (through cmd: PowerShell takes cargo's progress lines on stderr for errors)
        if ($target) {
            if (Test-Path "$env:ProgramFiles\LLVM\bin") { $env:PATH = "$env:ProgramFiles\LLVM\bin;$env:PATH" }
            cmd /c "cargo build --release --target $target -p eudora-app -p eudora-cli -p eudora-web 2>&1"
        } else {
            cmd /c "cargo build --release -p eudora-app -p eudora-cli -p eudora-web 2>&1"
        }
        if ($LASTEXITCODE -ne 0) { throw 'The build failed.' }
    } finally { Pop-Location }
}

Write-Output 'Step 2 of 3: staging the files'
$build = if ($target) { Join-Path $root "target\$target\release" } else { Join-Path $root 'target\release' }
$stage = Join-Path $root 'target\installer\stage'
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force $stage | Out-Null
foreach ($name in 'eudora.exe', 'eudora-cli.exe', 'eudora-web.exe') {
    $from = Join-Path $build $name
    if (-not (Test-Path $from)) { throw "$from isn't built (run without -NoBuild)" }
    Copy-Item $from $stage
}
Copy-Item (Join-Path $root 'crates\eudora-app\icons\icon.ico') $stage
foreach ($name in 'LICENSE-MIT', 'LICENSE-APACHE', 'THIRD-PARTY-NOTICES.md') { Copy-Item (Join-Path $root $name) $stage }
if (-not (Test-Path (Join-Path $root 'packaging\images\wizard-164x314.png'))) {
    & (Join-Path $PSScriptRoot 'make_installer_images.ps1')
}

Write-Output 'Step 3 of 3: making the installer'
$output = Join-Path $root 'dist'
New-Item -ItemType Directory -Force $output | Out-Null
Push-Location (Join-Path $root 'packaging')
try {
    & $iscc /Q "/DAppVersion=$version" "/DStage=$stage" "/DOutput=$output" "/DArch=$Arch" Eudora.iss
    if ($LASTEXITCODE -ne 0) { throw 'Inno Setup failed.' }
} finally { Pop-Location }
$suffix = if ($Arch -eq 'arm64') { '-arm64' } else { '' }
$setup = Join-Path $output "Eudora-Setup-$version$suffix.exe"
Write-Output ("Installer: $setup ({0:N1} MB)" -f ((Get-Item $setup).Length / 1MB))
