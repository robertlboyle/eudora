# Puts the Eudora just built into a fixed place to run and pin (%LOCALAPPDATA%\Programs\Eudora 27),
# with a Start Menu shortcut "Eudora" that opens the library; then starts it again.
#
# Why (2026-10-21): the taskbar showed a generic icon after builds. Running the build's own
# target\release\eudora27.exe, Windows read its icon while a build was rewriting it. Now a build
# never touches the copy that runs, and the copy is replaced in one step (a running program can be
# renamed, never overwritten), so it's never missing or half-written. Builds no longer wait for
# Eudora to close either.
#
#   .\scripts\install-dev.ps1                      # copy, shortcut, restart Eudora on its library
#   .\scripts\install-dev.ps1 -Library D:\Other    # the shortcut names this library
#   .\scripts\install-dev.ps1 -NoStart             # copy only
#
# Without -Library the shortcut names none, so Eudora opens the library it remembers (the last one
# opened by name, or restored: 2026-10-27, a shortcut naming C:\EudoraTest\library overrode a restore),
# else the usual place, as the installer's shortcut does.
param(
    [string]$Library = '',
    [switch]$NoStart
)
$ErrorActionPreference = 'Stop'

$build = Join-Path (Split-Path $PSScriptRoot) 'target\release'
$dest = Join-Path $env:LOCALAPPDATA 'Programs\Eudora 27'
New-Item -ItemType Directory -Force $dest | Out-Null

foreach ($name in 'eudora27.exe', 'eudora.exe') {
    $from = Join-Path $build $name
    if (-not (Test-Path $from)) { throw "$from isn't built yet (cargo build --release -p eudora-app -p eudora-cli)" }
    $to = Join-Path $dest $name
    # the new copy written in full first, then put in place by renaming
    Copy-Item $from "$to.new" -Force
    if (Test-Path $to) { Move-Item $to "$to.$(Get-Date -Format yyyyMMdd-HHmmss).old" }
    Move-Item "$to.new" $to
}
# old copies still running are left until they stop
Get-ChildItem $dest -Filter '*.old' | ForEach-Object { Remove-Item $_.FullName -Force -ErrorAction SilentlyContinue }

# the Start Menu shortcut (pin this one: it names the library)
$exe = Join-Path $dest 'eudora27.exe'
$link = Join-Path ([Environment]::GetFolderPath('Programs')) 'Eudora.lnk'
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($link)
$shortcut.TargetPath = $exe
$shortcut.Arguments = if ($Library) { "`"$Library`"" } else { '' }
$shortcut.WorkingDirectory = $dest
$shortcut.IconLocation = "$exe,0"
$shortcut.Description = 'Eudora'
$shortcut.Save()
# (the shortcut was "Eudora 27" before 2026-10-25: that one goes)
$old = Join-Path ([Environment]::GetFolderPath('Programs')) 'Eudora 27.lnk'
if (Test-Path $old) { Remove-Item $old -Force }
Write-Output "Installed in $dest; Start Menu shortcut: $link"

# .eml and .ics open with Eudora (as the installer registers them, for this user): listed under
# Open with and in Windows' Default apps, where the user picks it once
$classes = 'HKCU:\Software\Classes'
function Set-Value($key, $name, $value) {
    if (-not (Test-Path $key)) { New-Item -Path $key -Force | Out-Null }
    if ($name -eq '') { Set-Item -Path $key -Value $value } else { New-ItemProperty -Path $key -Name $name -Value $value -PropertyType String -Force | Out-Null }
}
foreach ($kind in @(@{ Ext = '.eml'; Name = 'Email message' }, @{ Ext = '.ics'; Name = 'Calendar file' })) {
    $progId = 'Eudora' + $kind.Ext
    Set-Value "$classes\$progId" '' $kind.Name
    Set-Value "$classes\$progId\DefaultIcon" '' "$exe,0"
    Set-Value "$classes\$progId\shell\open\command" '' "`"$exe`" `"%1`""
    Set-Value "$classes\$($kind.Ext)\OpenWithProgids" $progId ''
    Set-Value "$classes\Applications\eudora27.exe\SupportedTypes" $kind.Ext ''
    Set-Value 'HKCU:\Software\Planet Networks\Eudora\Capabilities\FileAssociations' $kind.Ext $progId
}
# mailto: links: offered in
# Default apps as the mail program; Windows lets the user choose it there
Set-Value "$classes\Eudora.mailto" '' 'URL:MailTo Protocol'
Set-Value "$classes\Eudora.mailto" 'URL Protocol' ''
Set-Value "$classes\Eudora.mailto\DefaultIcon" '' "$exe,0"
Set-Value "$classes\Eudora.mailto\shell\open\command" '' "`"$exe`" `"%1`""
Set-Value 'HKCU:\Software\Planet Networks\Eudora\Capabilities\URLAssociations' 'mailto' 'Eudora.mailto'
Set-Value 'HKCU:\Software\Clients\Mail\Eudora' '' 'Eudora'
Set-Value 'HKCU:\Software\Clients\Mail\Eudora\Capabilities' 'ApplicationName' 'Eudora'
Set-Value 'HKCU:\Software\Clients\Mail\Eudora\Capabilities\URLAssociations' 'mailto' 'Eudora.mailto'
Set-Value 'HKCU:\Software\Planet Networks\Eudora\Capabilities' 'ApplicationName' 'Eudora'
Set-Value 'HKCU:\Software\Planet Networks\Eudora\Capabilities' 'ApplicationDescription' 'Mail for decades of messages'
Set-Value 'HKCU:\Software\RegisteredApplications' 'Eudora' 'Software\Planet Networks\Eudora\Capabilities'

if (-not $NoStart) {
    Get-Process eudora27 -ErrorAction SilentlyContinue | Stop-Process
    # the window's background download too (stopping the window this way leaves it running, on
    # the old program): the window starts the new one within 30 s, and it resumes where it was
    # (the library's own copy of eudora.exe, in <library>\bin: any library's)
    Get-Process eudora -ErrorAction SilentlyContinue | Where-Object { $_.Path -like '*\bin\eudora.exe' -and $_.Path -ne (Join-Path $dest 'eudora.exe') } | Stop-Process
    Start-Sleep -Seconds 2
    if ($Library) { Start-Process $exe -ArgumentList "`"$Library`"" -WorkingDirectory $dest } else { Start-Process $exe -WorkingDirectory $dest }
    Start-Sleep -Seconds 6
    $running = Get-Process eudora27 -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe }
    if ($running) { Write-Output "Eudora is running from $exe (process $($running.Id -join ', '))." } else { throw 'Eudora did not start.' }
}
