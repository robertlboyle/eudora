; Eudora installer (Inno Setup 6, as PlanetPDF's). Built by scripts\build-installer.ps1,
; which passes /DAppVersion=… /DStage=… /DOutput=….
;
; Installs for the user who runs it (no administrator; %LOCALAPPDATA%\Programs\Eudora, where
; scripts\install-dev.ps1 puts development builds too), or for everyone when chosen. The mail
; library is never part of the install: it stays where it is (the one last opened is remembered,
; else %LOCALAPPDATA%\Eudora\Library), and uninstalling keeps it unless the user asks, typing
; DELETE, at the end of uninstalling (see "Deleting the user's data" in [Code]).
; Silent install for IT: /VERYSILENT /SUPPRESSMSGBOXES (/ALLUSERS for every user).

#ifndef AppVersion
  #error Run scripts\build-installer.ps1 (AppVersion not given)
#endif

#define AppName "Eudora"
#define Publisher "Planet Networks"

[Setup]
AppId={{B4E5D2C1-7A3F-4E8B-9C61-2F0D8A5E7B34}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#Publisher}
VersionInfoVersion={#AppVersion}
VersionInfoCompany={#Publisher}
VersionInfoDescription={#AppName} Setup
; The folder was "Eudora 27" before 1.0.0 (the program eudora27.exe, the command line eudora.exe):
; an update installs into "Eudora" (not that folder) and removes the old one; 1.0.0 and later are
; upgraded in their own folder (DefaultDir, Upgrading below).
DefaultDirName={code:DefaultDir}
UsePreviousAppDir=no
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
; x64 (Intel / AMD; also runs emulated on ARM64 Windows) or arm64 (Windows on ARM: Snapdragon
; and the like). Both share the AppId, so either replaces the other in place.
#ifndef Arch
  #define Arch "x64"
#endif
#if Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif
MinVersion=10.0
OutputDir={#Output}
#if Arch == "arm64"
OutputBaseFilename=Eudora-Setup-{#AppVersion}-arm64
#else
OutputBaseFilename=Eudora-Setup-{#AppVersion}
#endif
SetupIconFile={#Stage}\icon.ico
UninstallDisplayIcon={app}\eudora.exe
UninstallDisplayName={#AppName}
WizardStyle=modern
WizardImageFile=images\wizard-164x314.png,images\wizard-192x386.png,images\wizard-246x459.png,images\wizard-273x556.png,images\wizard-328x604.png,images\wizard-355x700.png,images\wizard-410x797.png
WizardSmallImageFile=images\small-55x55.png,images\small-64x68.png,images\small-69x69.png,images\small-83x80.png,images\small-92x97.png,images\small-110x106.png,images\small-119x123.png,images\small-138x140.png
Compression=lzma2/normal
SolidCompression=no
; Eudora's window is closed for the update (its background download runs from a copy in the
; library and takes the new program up by itself)
CloseApplications=yes
RestartApplications=no
ChangesEnvironment=yes
UsePreviousTasks=no
; (.eml, .msg, and .ics are registered below: Explorer is told to look again)
ChangesAssociations=yes

[Files]
Source: "{#Stage}\eudora.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\eudora-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\eudora-web.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\LICENSE-MIT"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\THIRD-PARTY-NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\eudora.exe"; Comment: "Mail for decades of messages"; AppUserModelID: "net.planet.eudora"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\eudora.exe"; Tasks: desktopicon; AppUserModelID: "net.planet.eudora"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"
Name: "addtopath"; Description: "Let command prompts and scripts run eudora-cli (adds Eudora to the PATH)"

[Registry]
; Eudora opens .eml messages and .ics calendar files, and Outlook .msg files in its message
; viewer: listed under Open with and in Windows' Default apps, where the user picks it (Windows
; lets no program make itself the default). HKA = this user's, or every user's when installed for
; everyone. (the old program's own entry, eudora27.exe before 1.0.0)
Root: HKA; Subkey: "Software\Classes\Applications\eudora27.exe"; Flags: deletekey dontcreatekey
Root: HKA; Subkey: "Software\Classes\Eudora.eml"; ValueType: string; ValueName: ""; ValueData: "Email message"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Eudora.eml\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\eudora.exe,0"
Root: HKA; Subkey: "Software\Classes\Eudora.eml\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\eudora.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\Eudora.msg"; ValueType: string; ValueName: ""; ValueData: "Outlook message"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Eudora.msg\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\eudora.exe,0"
Root: HKA; Subkey: "Software\Classes\Eudora.msg\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\eudora.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\Eudora.ics"; ValueType: string; ValueName: ""; ValueData: "Calendar file"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Eudora.ics\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\eudora.exe,0"
Root: HKA; Subkey: "Software\Classes\Eudora.ics\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\eudora.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\.eml\OpenWithProgids"; ValueType: string; ValueName: "Eudora.eml"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.ics\OpenWithProgids"; ValueType: string; ValueName: "Eudora.ics"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.msg\OpenWithProgids"; ValueType: string; ValueName: "Eudora.msg"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\Applications\eudora.exe\SupportedTypes"; ValueType: string; ValueName: ".eml"; ValueData: ""; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\eudora.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "Eudora"
Root: HKA; Subkey: "Software\Classes\Applications\eudora.exe\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\eudora.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\Applications\eudora.exe\SupportedTypes"; ValueType: string; ValueName: ".ics"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\eudora.exe\SupportedTypes"; ValueType: string; ValueName: ".msg"; ValueData: ""
Root: HKA; Subkey: "Software\Planet Networks\Eudora\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "Eudora"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Planet Networks\Eudora\Capabilities"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "Mail for decades of messages"
Root: HKA; Subkey: "Software\Planet Networks\Eudora\Capabilities\FileAssociations"; ValueType: string; ValueName: ".eml"; ValueData: "Eudora.eml"
Root: HKA; Subkey: "Software\Planet Networks\Eudora\Capabilities\FileAssociations"; ValueType: string; ValueName: ".ics"; ValueData: "Eudora.ics"
Root: HKA; Subkey: "Software\Planet Networks\Eudora\Capabilities\FileAssociations"; ValueType: string; ValueName: ".msg"; ValueData: "Eudora.msg"
; mailto: links: a URL protocol class, offered in Default apps as the mail program
Root: HKA; Subkey: "Software\Classes\Eudora.mailto"; ValueType: string; ValueName: ""; ValueData: "URL:MailTo Protocol"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Eudora.mailto"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Eudora.mailto\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\eudora.exe,0"
Root: HKA; Subkey: "Software\Classes\Eudora.mailto\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\eudora.exe"" ""%1"""
Root: HKA; Subkey: "Software\Planet Networks\Eudora\Capabilities\URLAssociations"; ValueType: string; ValueName: "mailto"; ValueData: "Eudora.mailto"
Root: HKA; Subkey: "Software\Clients\Mail\Eudora"; ValueType: string; ValueName: ""; ValueData: "Eudora"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Clients\Mail\Eudora\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "Eudora"
Root: HKA; Subkey: "Software\Clients\Mail\Eudora\Capabilities\URLAssociations"; ValueType: string; ValueName: "mailto"; ValueData: "Eudora.mailto"
Root: HKA; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "Eudora"; ValueData: "Software\Planet Networks\Eudora\Capabilities"; Flags: uninsdeletevalue

[InstallDelete]
; development builds' old copies (scripts\install-dev.ps1 renames the one replaced)
Type: files; Name: "{app}\*.old"
; the folder Eudora was installed in before 1.0.0 (its window is closed first: PrepareToInstall)
Type: filesandordirs; Name: "{autopf}\Eudora 27"

[Run]
; (as the user who ran Setup, never with the administrator rights an install for everyone has: 2026-11-11, the
; Eudora that 1.0.0's Setup started this way ran elevated on the library)
Filename: "{app}\eudora.exe"; Description: "Start Eudora"; Flags: nowait postinstall skipifsilent runasoriginaluser
; an update Eudora started (Restart Now: /SILENT … /relaunch=1; eudora-core/src/update.rs) starts
; Eudora again, as the user who ran it
Filename: "{app}\eudora.exe"; Flags: nowait runasoriginaluser; Check: Relaunch

[Code]
{ Started by Eudora's own update with Restart Now. }
function Relaunch: Boolean;
begin
  Result := ExpandConstant('{param:relaunch|0}') = '1';
end;

{ The PATH: the user's own (installed for me) or everyone's (installed for all users). }
function EnvRoot: Integer;
begin
  if IsAdminInstallMode then Result := HKEY_LOCAL_MACHINE else Result := HKEY_CURRENT_USER;
end;

function EnvKey: String;
begin
  if IsAdminInstallMode then
    Result := 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
  else
    Result := 'Environment';
end;

procedure AddToPath(Folder: String);
var
  Path: String;
begin
  if not RegQueryStringValue(EnvRoot, EnvKey, 'Path', Path) then
    Path := '';
  if Pos(';' + Uppercase(Folder) + ';', ';' + Uppercase(Path) + ';') > 0 then
    exit;
  if (Path <> '') and (Path[Length(Path)] <> ';') then
    Path := Path + ';';
  RegWriteExpandStringValue(EnvRoot, EnvKey, 'Path', Path + Folder);
end;

{ Takes Folder out of the PATH, if it's there. }
procedure RemoveFromPath(Folder: String);
var
  Path: String;
  P: Integer;
begin
  if not RegQueryStringValue(EnvRoot, EnvKey, 'Path', Path) then
    exit;
  P := Pos(';' + Uppercase(Folder) + ';', ';' + Uppercase(Path) + ';');
  if P = 0 then
    exit;
  Delete(Path, P - 1, Length(Folder) + 1);
  if (Length(Path) > 0) and (Path[1] = ';') then
    Delete(Path, 1, 1);
  RegWriteExpandStringValue(EnvRoot, EnvKey, 'Path', Path);
end;

{ Whether Folder is in the PATH. }
function InPath(Folder: String): Boolean;
var
  Path: String;
begin
  Result := RegQueryStringValue(EnvRoot, EnvKey, 'Path', Path) and (Pos(';' + Uppercase(Folder) + ';', ';' + Uppercase(Path) + ';') > 0);
end;

var
  { An Eudora from before 1.0.0 was replaced, and whether its folder was in the PATH (an update
    passes no tasks, /TASKS=, so what the old install had is carried over by hand). }
  ReplacedOld, OldOnPath: Boolean;

{ Eudora before 1.0.0 ran as eudora27.exe from the "Eudora 27" folder: its window is asked to
  close (it then stops its background download), else stopped, so the old folder can go. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code, Tries: Integer;
begin
  Result := '';
  OldOnPath := InPath(ExpandConstant('{autopf}\Eudora 27'));
  if not FileExists(ExpandConstant('{autopf}\Eudora 27\eudora27.exe')) then
  begin
    { (a PATH entry left by an old install whose folder is already gone goes too) }
    if OldOnPath then
      RemoveFromPath(ExpandConstant('{autopf}\Eudora 27'));
    exit;
  end;
  ReplacedOld := True;
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/IM eudora27.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Tries := 0;
  while (Tries < 20) and (Exec(ExpandConstant('{sys}\cmd.exe'), '/c tasklist /FI "IMAGENAME eq eudora27.exe" | find /I "eudora27.exe" >nul', '', SW_HIDE, ewWaitUntilTerminated, Code)) and (Code = 0) do
  begin
    Sleep(500);
    Tries := Tries + 1;
  end;
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM eudora27.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  RemoveFromPath(ExpandConstant('{autopf}\Eudora 27'));
end;

{ Upgrading. Every Eudora 1.0.0 or later installed
  here is found, for everyone (HKLM) and for this user (HKCU), whichever install mode this Setup
  runs in; the copy in use (a running eudora.exe, else the one installed last) is marked and
  chosen on an Upgrade page. Upgrading a copy installed in the other mode starts this Setup again
  in that mode (/ALLUSERS asks Windows for permission; /CURRENTUSER runs as the user) with
  /UPGRADE="folder", which picks it and skips the page. An upgrade skips the folder and task pages
  (its shortcuts and PATH entry stay as they are), and the Ready page says Upgrade. A silent
  update (the updater passes /ALLUSERS or /CURRENTUSER) upgrades the copy of its own mode. An
  install from before 1.0.0 ("Eudora 27") is not one: it goes to the new folder and the old one is
  removed, as above. }
var
  CopyDir, CopyVersion: array of String;
  CopyAdmin: array of Boolean;
  CopiesFound, Upgrade, Relaunching, Requested: Boolean;
  Chosen, InUse: Integer;
  UpgradePage: TInputOptionWizardPage;

function UninstallKey: String;
begin
  Result := 'Software\Microsoft\Windows\CurrentVersion\Uninstall\' + ExpandConstant('{#SetupSetting("AppId")}') + '_is1';
end;

procedure AddCopy(Root: Integer; Admin: Boolean);
var
  Dir, Version: String;
  N: Integer;
begin
  if not RegQueryStringValue(Root, UninstallKey, 'Inno Setup: App Path', Dir) then exit;
  { (from before 1.0.0, or a folder that's gone: nothing to upgrade there) }
  if (CompareText(ExtractFileName(RemoveBackslashUnlessRoot(Dir)), 'Eudora 27') = 0) or not FileExists(AddBackslash(Dir) + 'eudora.exe') then exit;
  RegQueryStringValue(Root, UninstallKey, 'DisplayVersion', Version);
  N := GetArrayLength(CopyDir);
  SetArrayLength(CopyDir, N + 1);
  SetArrayLength(CopyVersion, N + 1);
  SetArrayLength(CopyAdmin, N + 1);
  CopyDir[N] := RemoveBackslashUnlessRoot(Dir);
  CopyVersion[N] := Version;
  CopyAdmin[N] := Admin;
end;

{ The folder of a running eudora.exe, or ''. }
function RunningFolder: String;
var
  OutFile: String;
  Code: Integer;
  Text: AnsiString;
begin
  Result := '';
  OutFile := ExpandConstant('{tmp}\eudora-running.txt');
  if Exec('powershell.exe', '-NoProfile -NonInteractive -Command "(Get-Process eudora -ErrorAction SilentlyContinue | Where-Object Path | Select-Object -First 1).Path | Out-File -Encoding ascii ''' + OutFile + '''"', '', SW_HIDE, ewWaitUntilTerminated, Code)
    and LoadStringFromFile(OutFile, Text) and (Trim(String(Text)) <> '') then
    Result := RemoveBackslashUnlessRoot(ExtractFileDir(Trim(String(Text))));
end;

procedure FindCopies;
var
  I: Integer;
  Running, Asked: String;
  Newest, Stamp: Int64;
  Found: TFindRec;
begin
  if CopiesFound then exit;
  CopiesFound := True;
  AddCopy(HKLM, True);
  AddCopy(HKCU, False);
  InUse := -1;
  Chosen := -1;
  if GetArrayLength(CopyDir) = 0 then exit;
  Asked := RemoveBackslashUnlessRoot(ExpandConstant('{param:UPGRADE|}'));
  Running := RunningFolder;
  Newest := 0;
  for I := 0 to GetArrayLength(CopyDir) - 1 do
  begin
    if (Running <> '') and (CompareText(Running, CopyDir[I]) = 0) then
      InUse := I;
    if (Asked <> '') and (CompareText(Asked, CopyDir[I]) = 0) then
      Chosen := I;
  end;
  Requested := Chosen >= 0;
  if InUse < 0 then
    for I := 0 to GetArrayLength(CopyDir) - 1 do
      if FindFirst(AddBackslash(CopyDir[I]) + 'eudora.exe', Found) then
      begin
        Stamp := Int64(Found.LastWriteTime.dwHighDateTime) * 4294967296 + Int64(Found.LastWriteTime.dwLowDateTime);
        FindClose(Found);
        if Stamp > Newest then
        begin
          Newest := Stamp;
          InUse := I;
        end;
      end;
  if InUse < 0 then InUse := 0;
  { silent (the updater): the copy of this install mode }
  if (Chosen < 0) and WizardSilent then
    for I := 0 to GetArrayLength(CopyDir) - 1 do
      if CopyAdmin[I] = IsAdminInstallMode then
        Chosen := I;
  Upgrade := Chosen >= 0;
end;

function CopyCaption(I: Integer): String;
begin
  Result := 'Upgrade Eudora ' + CopyVersion[I] + ' in ' + CopyDir[I];
  if CopyAdmin[I] then Result := Result + ' (for everyone' else Result := Result + ' (for you only';
  if (I = InUse) and (GetArrayLength(CopyDir) > 1) then Result := Result + '; the copy you use)' else Result := Result + ')';
end;

function DefaultDir(Param: String): String;
var
  I: Integer;
begin
  FindCopies;
  if Chosen >= 0 then
    Result := CopyDir[Chosen]
  else
  begin
    Result := ExpandConstant('{autopf}\Eudora');
    for I := 0 to GetArrayLength(CopyDir) - 1 do
      if CopyAdmin[I] = IsAdminInstallMode then Result := CopyDir[I];
  end;
end;

procedure MakeUpgradePage;
var
  I: Integer;
  Sub: String;
begin
  FindCopies;
  if GetArrayLength(CopyDir) = 0 then exit;
  if GetArrayLength(CopyDir) > 1 then
    Sub := 'Eudora is installed twice on this computer. The copy you use is marked and chosen. Your mail, accounts, and settings are kept either way.'
  else
    Sub := 'Setup will upgrade it where it is. Your mail, accounts, and settings are kept.';
  UpgradePage := CreateInputOptionPage(wpWelcome, 'Upgrade Eudora', 'Eudora is already installed on this computer.', Sub, True, False);
  for I := 0 to GetArrayLength(CopyDir) - 1 do
    UpgradePage.Add(CopyCaption(I));
  UpgradePage.Add('Choose another folder instead');
  if Chosen >= 0 then UpgradePage.SelectedValueIndex := Chosen else UpgradePage.SelectedValueIndex := InUse;
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := False;
  if (UpgradePage <> nil) and (PageID = UpgradePage.ID) then
    { (started again for a chosen copy: already chosen) }
    Result := Requested
  else if (PageID = wpSelectDir) or (PageID = wpSelectTasks) then
    Result := Upgrade;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
var
  I, Code: Integer;
  Params: String;
begin
  Result := True;
  if (UpgradePage = nil) or (CurPageID <> UpgradePage.ID) then exit;
  I := UpgradePage.SelectedValueIndex;
  if I >= GetArrayLength(CopyDir) then
  begin
    Upgrade := False;
    Chosen := -1;
    exit;
  end;
  if CopyAdmin[I] <> IsAdminInstallMode then
  begin
    { that copy was installed in the other mode: Setup again, in that mode }
    Result := False;
    Params := '/UPGRADE="' + CopyDir[I] + '"';
    if CopyAdmin[I] then
      Relaunching := ShellExec('runas', ExpandConstant('{srcexe}'), '/ALLUSERS ' + Params, '', SW_SHOWNORMAL, ewNoWait, Code)
    else
      Relaunching := ExecAsOriginalUser(ExpandConstant('{srcexe}'), '/CURRENTUSER ' + Params, '', SW_SHOWNORMAL, ewNoWait, Code);
    if Relaunching then
      WizardForm.Close
    else
      MsgBox('Setup couldn''t start again to upgrade the copy in ' + CopyDir[I] + ': ' + SysErrorMessage(Code), mbError, MB_OK);
    exit;
  end;
  Chosen := I;
  Upgrade := True;
  WizardForm.DirEdit.Text := CopyDir[I];
end;

procedure CancelButtonClick(CurPageID: Integer; var Cancel, Confirm: Boolean);
begin
  if Relaunching then Confirm := False;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if (CurPageID = wpReady) and Upgrade then
  begin
    WizardForm.PageNameLabel.Caption := 'Ready to Upgrade';
    WizardForm.PageDescriptionLabel.Caption := Format('Eudora %s in %s will be upgraded to %s.', [CopyVersion[Chosen], CopyDir[Chosen], '{#AppVersion}']);
    WizardForm.ReadyLabel.Caption := 'Your mail, accounts, and settings are kept. Click Upgrade to continue.';
    WizardForm.NextButton.Caption := 'Upgrade';
  end;
end;

{ Eudora's window is drawn by Microsoft Edge WebView2, part of Windows 11 and of current
  Windows 10; on a computer without it, say where to get it. }
const WebView2Client = '{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}';

function HasWebView2: Boolean;
var
  Version: String;
begin
  Result :=
    (RegQueryStringValue(HKLM, 'SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\' + WebView2Client, 'pv', Version) and (Version <> '') and (Version <> '0.0.0.0')) or
    (RegQueryStringValue(HKLM, 'SOFTWARE\Microsoft\EdgeUpdate\Clients\' + WebView2Client, 'pv', Version) and (Version <> '') and (Version <> '0.0.0.0')) or
    (RegQueryStringValue(HKCU, 'Software\Microsoft\EdgeUpdate\Clients\' + WebView2Client, 'pv', Version) and (Version <> '') and (Version <> '0.0.0.0'));
end;

function InitializeSetup: Boolean;
begin
  Result := True;
  if not HasWebView2 then
    Result := MsgBox('Eudora needs Microsoft Edge WebView2, which this computer doesn''t have yet.' + #13#10#13#10 +
      'Get the "Evergreen Bootstrapper" from https://developer.microsoft.com/microsoft-edge/webview2/ and run it, then run this setup again.' + #13#10#13#10 +
      'Install Eudora anyway?', mbConfirmation, MB_YESNO) = IDYES;
end;

// "Did you know?" tips on the Installing page, while the files are copied (as PlanetPDF's):
// a new one every TipSeconds, or at once with "Next tip". Keep them true when features change.
const TipSeconds = 8;

var
  Tips: array of String;
  CurrentTip: Integer;
  TipShownAt: DWORD;
  TipHeading, TipText, TipCounter, TipNext: TNewStaticText;

function GetTickCount: DWORD; external 'GetTickCount@kernel32.dll stdcall';

procedure AddTip(Text: String);
begin
  SetArrayLength(Tips, GetArrayLength(Tips) + 1);
  Tips[GetArrayLength(Tips) - 1] := Text;
end;

procedure ShowTip(Index: Integer);
begin
  CurrentTip := Index mod GetArrayLength(Tips);
  TipText.Caption := Tips[CurrentTip];
  TipCounter.Caption := Format('Tip %d of %d', [CurrentTip + 1, GetArrayLength(Tips)]);
  TipShownAt := GetTickCount;
end;

procedure NextTipClick(Sender: TObject);
begin
  ShowTip(CurrentTip + 1);
end;

procedure InitializeWizard;
var
  Page: TNewNotebookPage;
  Gauge: TNewProgressBar;
begin
  AddTip('One library for every account: Microsoft 365, Outlook.com, Gmail, IMAP and POP3, and your old Eudora mailboxes, all in one window with one search.');
  AddTip('Eudora is built for huge mailboxes: millions of messages and decades of mail stay fast to open, scroll and search.');
  AddTip('Search understands Outlook''s words: from:, to:, subject:, hasattachment:yes, received:last week, size:>5MB, and any header, like importance:high.');
  AddTip('The last word you type in Search also finds words that begin with it (walkerfi finds walkerfield.example), and a word ending in * matches only by its start.');
  AddTip('The spam filter sorts arriving mail into Marketing, Notifications, Lists and Spam, and learns from every Spam and Not Spam you mark.');
  AddTip('Right-click any message and choose "Why?" to see exactly which rule sorted it, then switch that rule off or give it an exception in Mail Filter Rules.');
  AddTip('Views gather Receipts, Invoices, Payments, Travel, Legal, Verification Codes and Red Alerts from every account, without moving a single message.');
  AddTip('Red Alert keeps the messages you must never miss in sight: shut-off and past-due notices, government letters, security advisories and urgent mail from people you know.');
  AddTip('Flights, trains, hotels and tickets in your mail go onto your calendar by themselves, with the boarding pass or ticket attached for your phone.');
  AddTip('Messages that look like fraud get a red warning, and their links and pictures are switched off so they can''t be clicked by mistake.');
  AddTip('Pick the look you like: Eudora Classic, Eudora Modern, Thunderbird-style, Lookout classic or new, Gmail-style or Yahoo-style, in light or dark.');
  AddTip('Eudora Classic mode brings back Eudora 7''s menus, names and keyboard shortcuts, for everyone who loved the original.');
  AddTip('Calendar, Contacts and To Do sit beside your mail, and stay in step with Microsoft 365 and Outlook.');
  AddTip('Flag any message as To Do (Ctrl+Shift+G): the task opens the message again with one click, on any computer.');
  AddTip('Send Later waits in the Out mailbox and goes by itself at the time you chose, even if you close Eudora.');
  AddTip('File > Back Up Eudora puts your whole setup in one file (with a password if you like), to move it to a new computer, Windows or Mac.');
  AddTip('Everything Eudora does can be scripted: type "eudora-cli --help" in a command prompt. Programs and plugins only get the permissions you grant them.');
  AddTip('Work Offline keeps everything you do (moves, flags, replies) and sends it to the servers once you''re back online.');

  Page := WizardForm.InstallingPage;
  Gauge := WizardForm.ProgressGauge;

  TipHeading := TNewStaticText.Create(WizardForm);
  TipHeading.Parent := Page;
  TipHeading.Left := Gauge.Left;
  TipHeading.Top := Gauge.Top + Gauge.Height + ScaleY(28);
  TipHeading.Caption := 'Did you know?';
  TipHeading.Font.Style := [fsBold];

  TipText := TNewStaticText.Create(WizardForm);
  TipText.Parent := Page;
  TipText.ShowAccelChar := False;
  TipText.Left := Gauge.Left;
  TipText.Top := TipHeading.Top + TipHeading.Height + ScaleY(6);
  TipText.AutoSize := False;
  TipText.WordWrap := True;
  TipText.Width := Gauge.Width;
  TipText.Height := ScaleY(64);

  TipCounter := TNewStaticText.Create(WizardForm);
  TipCounter.Parent := Page;
  TipCounter.Left := Gauge.Left;
  TipCounter.Top := TipText.Top + TipText.Height + ScaleY(4);
  TipCounter.Font.Color := clGray;

  TipNext := TNewStaticText.Create(WizardForm);
  TipNext.Parent := Page;
  TipNext.Caption := 'Next tip ›';
  TipNext.Font.Color := clBlue;
  TipNext.Font.Style := [fsUnderline];
  TipNext.Cursor := crHand;
  TipNext.OnClick := @NextTipClick;
  TipNext.Top := TipCounter.Top;
  TipNext.Left := Gauge.Left + Gauge.Width - TipNext.Width;

  ShowTip(0);
  MakeUpgradePage;
end;

procedure CurInstallProgressChanged(CurProgress, MaxProgress: Integer);
begin
  if GetTickCount - TipShownAt >= TipSeconds * 1000 then
    ShowTip(CurrentTip + 1);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
    ShowTip(0);
  if (CurStep = ssPostInstall) and (WizardIsTaskSelected('addtopath') or OldOnPath) then
    AddToPath(ExpandConstant('{app}'));
  { the old version's desktop shortcut, pointing at eudora27.exe: made to point at Eudora again }
  if (CurStep = ssPostInstall) and ReplacedOld and FileExists(ExpandConstant('{autodesktop}\Eudora.lnk')) and not WizardIsTaskSelected('desktopicon') then
    CreateShellLink(ExpandConstant('{autodesktop}\Eudora.lnk'), 'Mail for decades of messages', ExpandConstant('{app}\eudora.exe'), '', ExpandConstant('{app}'), '', 0, SW_SHOWNORMAL);
end;

{ Deleting the user's data. At the end of uninstalling, when no other Eudora stays installed
  (that one would still use them) and the uninstall isn't silent, Eudora asks whether to delete
  this user's mail library, Eudora's folders, and its saved passwords. Keep is the default; Delete
  works only once DELETE is typed. Kept, a reinstall finds everything as it was. }
var
  DeleteButton: TNewButton;
  DeleteWord: TNewEdit;

{ The library Eudora remembers (last-library.txt) when it isn't in Eudora's own folder, or ''. }
function RememberedLibrary: String;
var
  Text: AnsiString;
  Path: String;
begin
  Result := '';
  if not LoadStringFromFile(ExpandConstant('{localappdata}\Eudora\last-library.txt'), Text) then exit;
  Path := RemoveBackslashUnlessRoot(Trim(String(Text)));
  { (only a real library, and never a drive's root) }
  if (Length(Path) > 3) and FileExists(AddBackslash(Path) + 'eudora.db')
    and (Pos(Uppercase(AddBackslash(ExpandConstant('{localappdata}\Eudora'))), Uppercase(AddBackslash(Path))) <> 1) then
    Result := Path;
end;

procedure DeleteWordChanged(Sender: TObject);
begin
  DeleteButton.Enabled := Trim(DeleteWord.Text) = 'DELETE';
end;

{ Asks; True = delete. Folders: one per line. }
function AskToDelete(Folders: String): Boolean;
var
  Form: TSetupForm;
  Text, Prompt: TNewStaticText;
  KeepButton: TNewButton;
  Gap, Y: Integer;
begin
  Form := CreateCustomForm(ScaleX(520), ScaleY(100), False, False);
  try
    Form.Caption := 'Eudora Uninstall';
    Gap := ScaleX(12);
    Text := TNewStaticText.Create(Form);
    Text.Parent := Form;
    Text.Left := Gap;
    Text.Top := Gap;
    Text.Width := Form.ClientWidth - 2 * Gap;
    Text.WordWrap := True;
    Text.Caption := 'Eudora has been removed. Your mail library, accounts, rules, settings, and saved passwords are still on this computer, so installing Eudora again brings everything back as it was.' + #13#10#13#10 +
      'Do you also want to delete them? This can''t be undone. Mail kept only on this computer (POP3 accounts, imported files, and old Eudora mailboxes) will be gone for good. Mail on Microsoft 365, Gmail, and IMAP servers stays on those servers.' + #13#10#13#10 +
      'These would be deleted:' + #13#10 + Folders +
      '   Eudora''s saved passwords and sign-ins in Windows Credential Manager';
    Text.AdjustHeight;
    Y := Text.Top + Text.Height + Gap;

    DeleteWord := TNewEdit.Create(Form);
    Prompt := TNewStaticText.Create(Form);
    Prompt.Parent := Form;
    Prompt.Left := Gap;
    Prompt.Top := Y;
    Prompt.Caption := '&To delete them, type DELETE here:';
    Prompt.FocusControl := DeleteWord;
    Y := Y + Prompt.Height + ScaleY(4);

    DeleteWord.Parent := Form;
    DeleteWord.Left := Gap;
    DeleteWord.Top := Y;
    DeleteWord.Width := ScaleX(200);
    DeleteWord.OnChange := @DeleteWordChanged;
    Y := Y + DeleteWord.Height + Gap * 2;

    KeepButton := TNewButton.Create(Form);
    KeepButton.Parent := Form;
    KeepButton.Caption := '&Keep My Data';
    KeepButton.Width := ScaleX(120);
    KeepButton.Height := ScaleY(25);
    KeepButton.Left := Form.ClientWidth - Gap - KeepButton.Width;
    KeepButton.Top := Y;
    KeepButton.ModalResult := mrCancel;
    KeepButton.Default := True;
    KeepButton.Cancel := True;

    DeleteButton := TNewButton.Create(Form);
    DeleteButton.Parent := Form;
    DeleteButton.Caption := '&Delete';
    DeleteButton.Width := ScaleX(120);
    DeleteButton.Height := ScaleY(25);
    DeleteButton.Left := KeepButton.Left - ScaleX(8) - DeleteButton.Width;
    DeleteButton.Top := Y;
    DeleteButton.ModalResult := mrOk;
    DeleteButton.Enabled := False;

    Form.ClientHeight := Y + KeepButton.Height + Gap;
    Form.ActiveControl := KeepButton;
    Result := (Form.ShowModal = mrOk) and (Trim(DeleteWord.Text) = 'DELETE');
  finally
    Form.Free;
  end;
end;

procedure DeleteUserData;
var
  Dirs: array of String;
  List, Left: String;
  I, Code: Integer;
begin
  { (another Eudora still installed, for everyone or for this user: it still uses them) }
  if UninstallSilent or RegKeyExists(HKLM, UninstallKey) or RegKeyExists(HKCU, UninstallKey) then exit;
  if DirExists(ExpandConstant('{localappdata}\Eudora')) then
  begin
    SetArrayLength(Dirs, GetArrayLength(Dirs) + 1);
    Dirs[GetArrayLength(Dirs) - 1] := RememberedLibrary;
    if Dirs[GetArrayLength(Dirs) - 1] = '' then SetArrayLength(Dirs, 0);
    SetArrayLength(Dirs, GetArrayLength(Dirs) + 1);
    Dirs[GetArrayLength(Dirs) - 1] := ExpandConstant('{localappdata}\Eudora');
  end;
  if DirExists(ExpandConstant('{localappdata}\net.planet.eudora')) then
  begin
    SetArrayLength(Dirs, GetArrayLength(Dirs) + 1);
    Dirs[GetArrayLength(Dirs) - 1] := ExpandConstant('{localappdata}\net.planet.eudora');
  end;
  if GetArrayLength(Dirs) = 0 then exit;
  List := '';
  for I := 0 to GetArrayLength(Dirs) - 1 do
    List := List + '   ' + Dirs[I] + #13#10;
  if not AskToDelete(List) then exit;

  { a download Eudora left running would hold the library's files }
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM eudora-cli.exe /IM eudora.exe /FI "USERNAME eq ' + GetUserNameString + '"', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(1000);
  { saved passwords and sign-ins: Credential Manager entries named "<name>.Eudora" (and the old "<name>.Eudora 27") }
  Exec('powershell.exe', '-NoProfile -NonInteractive -Command "cmdkey /list | ForEach-Object { if ($_ -match ''target=(.+\.Eudora( 27)?)\s*$'') { cmdkey /delete:$($matches[1]) | Out-Null } }"', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Left := '';
  for I := 0 to GetArrayLength(Dirs) - 1 do
  begin
    DelTree(Dirs[I], True, True, True);
    if DirExists(Dirs[I]) then Left := Left + '   ' + Dirs[I] + #13#10;
  end;
  if Left <> '' then
    MsgBox('Some files couldn''t be deleted (another program may be using them). Delete these folders yourself:' + #13#10#13#10 + Left, mbError, MB_OK)
  else
    MsgBox('Your Eudora mail library, settings, and saved passwords have been deleted from this computer.', mbInformation, MB_OK);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    RemoveFromPath(ExpandConstant('{app}'));
    DeleteUserData;
  end;
end;
