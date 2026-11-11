# Builds Eudora for Android: dist\Eudora-<version>-android.apk (ARM64 phones).
#
#   .\scripts\build-android.ps1           # release: optimized, signed with Eudora's Android key
#   .\scripts\build-android.ps1 -Debug    # debug build (unoptimized, large), for trying things out
#
# Needs the Android SDK, NDK 29, JDK 17 and tauri-cli (BUILDING.md ▸ Android). The signing
# key is kept outside the project; EUDORA_ANDROID_SIGNING names its properties file (else
# the release APK is left unsigned). Steps (counted): 1 the environment, 2 the build, 3 the APK.
#   .\scripts\build-android.ps1 -Debug -Target x86_64   # for the emulator (an x86_64 Android)
param([switch]$Debug, [ValidateSet('aarch64', 'x86_64')][string]$Target = 'aarch64')
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot

Write-Output 'Step 1 of 3: the Android tools'
$sdk = if ($env:ANDROID_HOME) { $env:ANDROID_HOME } else { Join-Path $env:LOCALAPPDATA 'Android\Sdk' }
if (-not (Test-Path $sdk)) { throw "No Android SDK at $sdk (BUILDING.md ▸ Android)." }
$env:ANDROID_HOME = $sdk
if (-not $env:NDK_HOME) {
    $ndk = Get-ChildItem (Join-Path $sdk 'ndk') -Directory | Sort-Object Name | Select-Object -Last 1
    if (-not $ndk) { throw 'No NDK in the SDK (sdkmanager "ndk;29.0.14206865").' }
    $env:NDK_HOME = $ndk.FullName
}
if (-not $env:JAVA_HOME) {
    $jdk = Get-ChildItem 'C:\Program Files\Microsoft' -Directory -Filter 'jdk-17*' | Select-Object -First 1
    if (-not $jdk) { throw 'No JDK 17 (winget install Microsoft.OpenJDK.17).' }
    $env:JAVA_HOME = $jdk.FullName
}
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:JAVA_HOME\bin;$env:PATH"
# (Java's internal connection needs a short folder for its socket: a long temp path breaks it)
$short = Join-Path $env:USERPROFILE '.jt'
New-Item -ItemType Directory -Force $short | Out-Null
$env:JAVA_TOOL_OPTIONS = "-Djdk.net.unixdomain.tmpdir=$short"
if (-not $env:EUDORA_ANDROID_SIGNING) {
    $env:EUDORA_ANDROID_SIGNING = (Get-ItemProperty HKCU:\Environment -ErrorAction SilentlyContinue).EUDORA_ANDROID_SIGNING
}
if (-not $Debug -and -not ($env:EUDORA_ANDROID_SIGNING -and (Test-Path $env:EUDORA_ANDROID_SIGNING))) {
    Write-Output '  (no signing key found: the release APK will be unsigned and won''t install)'
}
# A phone needs no symbols or line tables (desktop builds keep them for crash reports): without
# them the app's library is a fraction of the size
$env:CARGO_PROFILE_RELEASE_DEBUG = 'false'
$env:CARGO_PROFILE_RELEASE_STRIP = 'symbols'

Write-Output 'Step 2 of 3: building (Rust, then Gradle)'
$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
Push-Location (Join-Path $root 'crates\eudora-app')
try {
    $build = @('tauri', 'android', 'build', '--apk', '--target', $Target, '--ci')
    if ($Debug) { $build += '--debug' }
    # (Cargo writes its progress to stderr: in Windows PowerShell that's an "error" when the
    # output is redirected, so the stop-on-error rule is lifted for this one command)
    $ErrorActionPreference = 'Continue'
    & cargo @build 2>&1 | ForEach-Object { "$_" }
    $code = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($code -ne 0) { throw 'The Android build failed.' }
} finally { Pop-Location }

Write-Output 'Step 3 of 3: the APK'
$kind = if ($Debug) { 'debug' } else { 'release' }
$apk = Join-Path $root "crates\eudora-app\gen\android\app\build\outputs\apk\universal\$kind\app-universal-$kind.apk"
$out = Join-Path $root 'dist'
New-Item -ItemType Directory -Force $out | Out-Null
$name = if ($Debug) { "Eudora-$version-android-debug.apk" } else { "Eudora-$version-android.apk" }
Copy-Item $apk (Join-Path $out $name) -Force
Write-Output ("APK: {0} ({1:N1} MB)" -f (Join-Path $out $name), ((Get-Item (Join-Path $out $name)).Length / 1MB))
