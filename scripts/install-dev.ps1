# Puts the Eudora just built into a fixed place to run and pin (%LOCALAPPDATA%\Programs\Eudora),
# with a Start Menu shortcut "Eudora" that opens the library; then starts it again.
#
# Why (2026-10-21): the taskbar showed a generic icon after builds. Running the build's own
# target\release program, Windows read its icon while a build was rewriting it. Now a build
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
#
# Before 1.0.0 the window was eudora27.exe and the command line eudora.exe, in Programs\Eudora 27;
# now the window is eudora.exe and the command line eudora-cli.exe, in Programs\Eudora. The old
# folder and its registrations are removed here, as the installer does.
#
# Never inside another app's container: run from a packaged app, Windows quietly redirects every
# write to %LOCALAPPDATA% into that app's own private copy, and the Eudora it starts would use a
# library nobody else sees. So when this script finds itself in a package's context it starts
# itself again outside (through Windows' management service, in the user's own session), waits,
# and shows what that run printed.
param(
    [string]$Library = '',
    [switch]$NoStart,
    [string]$OutsideLog = ''
)
$ErrorActionPreference = 'Stop'

# (a probe file: Windows' own "am I in a package" answer can say no while writes are redirected;
# a redirected file lands in that app's LocalCache, where it shows)
$probe = "eudora-redirect-probe-$PID.tmp"
Set-Content (Join-Path $env:LOCALAPPDATA $probe) 'x'
$redirected = [bool](Get-ChildItem (Join-Path $env:LOCALAPPDATA "Packages\*\LocalCache\Local\$probe") -ErrorAction SilentlyContinue)
Remove-Item (Join-Path $env:LOCALAPPDATA $probe) -ErrorAction SilentlyContinue
if ($redirected) {
    $log = Join-Path (Split-Path $PSScriptRoot) 'target\install-dev-outside.log'
    Remove-Item $log -ErrorAction SilentlyContinue
    $arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -OutsideLog `"$log`""
    if ($Library) { $arguments += " -Library `"$Library`"" }
    if ($NoStart) { $arguments += ' -NoStart' }
    Write-Output 'Running inside another app''s container (its AppData is redirected): installing from outside it instead.'
    $started = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = "powershell $arguments" }
    if ($started.ReturnValue -ne 0) { throw "Couldn't start the install outside the container (Windows answered $($started.ReturnValue))." }
    $deadline = (Get-Date).AddMinutes(3)
    while (-not ((Test-Path $log) -and ((Get-Content $log -Raw) -match '(?m)^(END|FAILED)'))) {
        if ((Get-Date) -gt $deadline) { throw "The install outside the container didn't finish within 3 minutes (see $log)." }
        Start-Sleep -Milliseconds 500
    }
    $text = Get-Content $log
    $text | Where-Object { $_ -notmatch '^(END|FAILED)' } | Write-Output
    if ($text -match '^FAILED') { throw ($text | Where-Object { $_ -match '^FAILED' } | Select-Object -First 1) }
    return
}
if ($OutsideLog) {
    Start-Transcript -Path "$OutsideLog.transcript" -Force | Out-Null
    trap { "FAILED: $_" | Add-Content $OutsideLog; Stop-Transcript | Out-Null; exit 1 }
}
function Say($text) { Write-Output $text; if ($OutsideLog) { $text | Add-Content $OutsideLog } }

$build = Join-Path (Split-Path $PSScriptRoot) 'target\release'
$dest = Join-Path $env:LOCALAPPDATA 'Programs\Eudora'
$oldDest = Join-Path $env:LOCALAPPDATA 'Programs\Eudora 27'
New-Item -ItemType Directory -Force $dest | Out-Null

foreach ($name in 'eudora.exe', 'eudora-cli.exe') {
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
$exe = Join-Path $dest 'eudora.exe'
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
Say "Installed in $dest; Start Menu shortcut: $link"

# .eml, .msg (in the message viewer), and .ics open with Eudora (as the installer registers them,
# for this user): listed under
# Open with and in Windows' Default apps, where the user picks it once
$classes = 'HKCU:\Software\Classes'
function Set-Value($key, $name, $value) {
    if (-not (Test-Path $key)) { New-Item -Path $key -Force | Out-Null }
    if ($name -eq '') { Set-Item -Path $key -Value $value } else { New-ItemProperty -Path $key -Name $name -Value $value -PropertyType String -Force | Out-Null }
}
# (the old program's own entry, eudora27.exe before 1.0.0)
if (Test-Path "$classes\Applications\eudora27.exe") { Remove-Item "$classes\Applications\eudora27.exe" -Recurse -Force }
foreach ($kind in @(@{ Ext = '.eml'; Name = 'Email message' }, @{ Ext = '.msg'; Name = 'Outlook message' }, @{ Ext = '.ics'; Name = 'Calendar file' })) {
    $progId = 'Eudora' + $kind.Ext
    Set-Value "$classes\$progId" '' $kind.Name
    Set-Value "$classes\$progId\DefaultIcon" '' "$exe,0"
    Set-Value "$classes\$progId\shell\open\command" '' "`"$exe`" `"%1`""
    Set-Value "$classes\$($kind.Ext)\OpenWithProgids" $progId ''
    Set-Value "$classes\Applications\eudora.exe\SupportedTypes" $kind.Ext ''
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
# (the name Open with shows, and Explorer told the associations changed, as the installer's
# ChangesAssociations does: without it, Open with didn't list Eudora for .msg until a sign-out)
Set-Value "$classes\Applications\eudora.exe" 'FriendlyAppName' 'Eudora'
Set-Value "$classes\Applications\eudora.exe\shell\open\command" '' "`"$exe`" `"%1`""
Add-Type -Namespace Eudora -Name Shell -MemberDefinition '[DllImport("shell32.dll")] public static extern void SHChangeNotify(int eventId, uint flags, System.IntPtr a, System.IntPtr b);'
[Eudora.Shell]::SHChangeNotify(0x08000000, 0, [IntPtr]::Zero, [IntPtr]::Zero)

if (-not $NoStart) {
    # the window (eudora.exe; eudora27.exe before 1.0.0), and its background download, which
    # stopping the window this way leaves running: the library's own copy of the command line in
    # <library>\bin (eudora-cli.exe; eudora.exe before 1.0.0). The window starts the new one within
    # 30 s, and it resumes where it was.
    Get-Process eudora27 -ErrorAction SilentlyContinue | Stop-Process
    Get-Process eudora -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe -or $_.Path -like '*\bin\eudora.exe' } | Stop-Process
    Get-Process eudora-cli -ErrorAction SilentlyContinue | Where-Object { $_.Path -like '*\bin\eudora-cli.exe' } | Stop-Process
    Start-Sleep -Seconds 2
}
# the old program folder (before 1.0.0), once nothing runs from it
if (Test-Path $oldDest) {
    Remove-Item $oldDest -Recurse -Force -ErrorAction SilentlyContinue
    if (Test-Path $oldDest) { Say "Couldn't remove $oldDest yet (something still runs from it): it goes next time." }
    else { Say "Removed the old program folder $oldDest" }
}
if (-not $NoStart) {
    if ($Library) { Start-Process $exe -ArgumentList "`"$Library`"" -WorkingDirectory $dest } else { Start-Process $exe -WorkingDirectory $dest }
    Start-Sleep -Seconds 6
    $running = Get-Process eudora -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe }
    if ($running) { Say "Eudora is running from $exe (process $($running.Id -join ', '))." } else { throw 'Eudora did not start.' }
}
if ($OutsideLog) { 'END' | Add-Content $OutsideLog; Stop-Transcript | Out-Null }
