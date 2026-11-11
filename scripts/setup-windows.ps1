# Sets up a Windows 10 or 11 computer to build Eudora (BUILDING.md). Checks what's installed and
# installs only what's missing, through winget (Windows' own package manager); each install may
# ask for an administrator's approval.
#
#   .\scripts\setup-windows.ps1              # the desktop version for this PC
#   .\scripts\setup-windows.ps1 -Arm64       # also build for Windows on ARM (ARM64)
#   .\scripts\setup-windows.ps1 -Installer   # also make the Windows installer (Inno Setup 6)
#   .\scripts\setup-windows.ps1 -Android     # also the Android version
#   .\scripts\setup-windows.ps1 -Yes         # don't ask before each install
param([switch]$Arm64, [switch]$Installer, [switch]$Android, [switch]$Yes)
$ErrorActionPreference = 'Stop'

$steps = @('Git', 'Visual Studio Build Tools (C++)', 'Rust')
if ($Arm64) { $steps += 'ARM64 compilers and LLVM' }
if ($Installer) { $steps += 'Inno Setup 6' }
if ($Android) { $steps += 'Android (JDK 17, Android Studio, Rust targets, Tauri CLI)' }
$step = 0
function Step($what) { $script:step++; Write-Output "Step $script:step of $($steps.Count): $what" }

if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
    throw 'winget is missing: install "App Installer" from the Microsoft Store, then run this again.'
}

function Confirm($what) {
    if ($Yes) { return $true }
    $answer = Read-Host "Install $what? [Y/n]"
    return ($answer -eq '' -or $answer -match '^[Yy]')
}

function WingetInstall($id, $what, $extra = @()) {
    if (-not (Confirm $what)) { Write-Output "  Skipped $what."; return }
    winget install --id $id -e --source winget --accept-package-agreements --accept-source-agreements @extra
    if ($LASTEXITCODE -ne 0) { throw "Installing $what failed (winget exit code $LASTEXITCODE)." }
}

function RefreshPath {
    $env:PATH = [Environment]::GetEnvironmentVariable('PATH', 'Machine') + ';' + [Environment]::GetEnvironmentVariable('PATH', 'User')
}

# 1. Git
Step 'Git'
if (Get-Command git -ErrorAction SilentlyContinue) { Write-Output "  Already there: $(git --version)" }
else { WingetInstall 'Git.Git' 'Git'; RefreshPath }

# 2. Visual Studio Build Tools: the C++ compiler and linker, and the Windows SDK
Step 'Visual Studio Build Tools (C++)'
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = if (Test-Path $vswhere) { & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -latest -property installationPath } else { $null }
if ($vs) { Write-Output "  Already there: $vs" }
else {
    WingetInstall 'Microsoft.VisualStudio.2022.BuildTools' 'Visual Studio 2022 Build Tools with the C++ workload (10-20 minutes)' `
        @('--override', '--wait --passive --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended')
    $vs = if (Test-Path $vswhere) { & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -latest -property installationPath } else { $null }
}

# 3. Rust (rustup, the stable toolchain)
Step 'Rust'
$rustup = Get-Command rustup -ErrorAction SilentlyContinue
if (-not $rustup -and (Test-Path "$env:USERPROFILE\.cargo\bin\rustup.exe")) { $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"; $rustup = $true }
if ($rustup) { Write-Output '  Already there; bringing stable Rust up to date'; rustup update stable }
else { WingetInstall 'Rustlang.Rustup' 'Rust (rustup)'; $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH" }
if (Get-Command rustup -ErrorAction SilentlyContinue) {
    rustup default stable
    Write-Output "  $(rustc --version)"
}

# 4. Windows on ARM: the ARM64 compilers, Rust's target, and LLVM's clang (the ring library)
if ($Arm64) {
    Step 'ARM64 compilers and LLVM'
    rustup target add aarch64-pc-windows-msvc
    if ($vs -and -not (Get-ChildItem "$vs\VC\Tools\MSVC\*\bin\Hostx64\arm64\cl.exe" -ErrorAction SilentlyContinue)) {
        if (Confirm 'the MSVC ARM64 build tools') {
            $setup = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\setup.exe"
            $p = Start-Process -FilePath $setup -ArgumentList 'modify', '--installPath', "`"$vs`"", '--add', 'Microsoft.VisualStudio.Component.VC.Tools.ARM64', '--passive', '--norestart' -Verb RunAs -Wait -PassThru
            if ($p.ExitCode -ne 0) { throw "Adding the ARM64 build tools failed (exit code $($p.ExitCode))." }
        }
    } else { Write-Output '  ARM64 compilers already there' }
    if (Test-Path "$env:ProgramFiles\LLVM\bin\clang.exe") { Write-Output '  LLVM already there' }
    else { WingetInstall 'LLVM.LLVM' 'LLVM (clang)' }
}

# 5. The installer maker
if ($Installer) {
    Step 'Inno Setup 6'
    $iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe") | Where-Object { Test-Path $_ }
    if ($iscc) { Write-Output '  Already there' } else { WingetInstall 'JRSoftware.InnoSetup' 'Inno Setup 6' }
}

# 6. Android
if ($Android) {
    Step 'Android (JDK 17, Android Studio, Rust targets, Tauri CLI)'
    if (Get-Command java -ErrorAction SilentlyContinue) { Write-Output '  Java already there' } else { WingetInstall 'Microsoft.OpenJDK.17' 'OpenJDK 17' }
    if (Test-Path "$env:LOCALAPPDATA\Android\Sdk") { Write-Output '  Android SDK already there' } else { WingetInstall 'Google.AndroidStudio' 'Android Studio' }
    rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android
    cargo install tauri-cli --version 2.12.4 --locked
    Write-Output @'
  Then, once: open Android Studio ▸ More Actions ▸ SDK Manager and install the Android SDK
  Platform 36, Build-Tools 36, Command-line Tools, and NDK 29; then set the user variables
  ANDROID_HOME (the Sdk folder), NDK_HOME (Sdk\ndk\<version>), and JAVA_HOME (the JDK 17 folder).
'@
}

Write-Output ''
Write-Output 'Done. Open a new PowerShell window (so the new PATH applies), then:'
Write-Output '  cargo build --release -p eudora-app -p eudora-cli'
