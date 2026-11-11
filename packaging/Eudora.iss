; Eudora 27 installer (Inno Setup 6, as PlanetPDF's). Built by scripts\build-installer.ps1,
; which passes /DAppVersion=… /DStage=… /DOutput=….
;
; Installs for the user who runs it (no administrator; %LOCALAPPDATA%\Programs\Eudora 27, where
; scripts\install-dev.ps1 puts development builds too), or for everyone when chosen. The mail
; library is never part of the install: it stays where it is (the one last opened is remembered,
; else %LOCALAPPDATA%\Eudora 27\Library), and uninstalling never removes it.
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
; (the folder keeps its first name, so updates replace what's there)
DefaultDirName={autopf}\Eudora 27
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
OutputBaseFilename=Eudora-27-Setup-{#AppVersion}-arm64
#else
OutputBaseFilename=Eudora-27-Setup-{#AppVersion}
#endif
SetupIconFile={#Stage}\icon.ico
UninstallDisplayIcon={app}\eudora27.exe
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
Source: "{#Stage}\eudora27.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\eudora.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\eudora-web.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\LICENSE-MIT"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\THIRD-PARTY-NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\eudora27.exe"; Comment: "Mail for decades of messages"; AppUserModelID: "net.planet.eudora27"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\eudora27.exe"; Tasks: desktopicon; AppUserModelID: "net.planet.eudora27"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"
Name: "addtopath"; Description: "Let command prompts and scripts run eudora (adds Eudora to the PATH)"

[Registry]
; Eudora opens .eml messages and .ics calendar files, and Outlook .msg files in its message
; viewer: listed under Open with and in Windows' Default apps, where the user picks it (Windows
; lets no program make itself the default). HKA = this user's, or every user's when installed for
; everyone.
Root: HKA; Subkey: "Software\Classes\Eudora.eml"; ValueType: string; ValueName: ""; ValueData: "Email message"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Eudora.eml\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\eudora27.exe,0"
Root: HKA; Subkey: "Software\Classes\Eudora.eml\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\eudora27.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\Eudora.msg"; ValueType: string; ValueName: ""; ValueData: "Outlook message"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Eudora.msg\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\eudora27.exe,0"
Root: HKA; Subkey: "Software\Classes\Eudora.msg\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\eudora27.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\Eudora.ics"; ValueType: string; ValueName: ""; ValueData: "Calendar file"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Eudora.ics\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\eudora27.exe,0"
Root: HKA; Subkey: "Software\Classes\Eudora.ics\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\eudora27.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\.eml\OpenWithProgids"; ValueType: string; ValueName: "Eudora.eml"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.ics\OpenWithProgids"; ValueType: string; ValueName: "Eudora.ics"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.msg\OpenWithProgids"; ValueType: string; ValueName: "Eudora.msg"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\Applications\eudora27.exe\SupportedTypes"; ValueType: string; ValueName: ".eml"; ValueData: ""; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\eudora27.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "Eudora"
Root: HKA; Subkey: "Software\Classes\Applications\eudora27.exe\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\eudora27.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\Applications\eudora27.exe\SupportedTypes"; ValueType: string; ValueName: ".ics"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\eudora27.exe\SupportedTypes"; ValueType: string; ValueName: ".msg"; ValueData: ""
Root: HKA; Subkey: "Software\Planet Networks\Eudora\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "Eudora"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Planet Networks\Eudora\Capabilities"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "Mail for decades of messages"
Root: HKA; Subkey: "Software\Planet Networks\Eudora\Capabilities\FileAssociations"; ValueType: string; ValueName: ".eml"; ValueData: "Eudora.eml"
Root: HKA; Subkey: "Software\Planet Networks\Eudora\Capabilities\FileAssociations"; ValueType: string; ValueName: ".ics"; ValueData: "Eudora.ics"
Root: HKA; Subkey: "Software\Planet Networks\Eudora\Capabilities\FileAssociations"; ValueType: string; ValueName: ".msg"; ValueData: "Eudora.msg"
; mailto: links: a URL protocol class, offered in Default apps as the mail program
Root: HKA; Subkey: "Software\Classes\Eudora.mailto"; ValueType: string; ValueName: ""; ValueData: "URL:MailTo Protocol"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Eudora.mailto"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Eudora.mailto\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\eudora27.exe,0"
Root: HKA; Subkey: "Software\Classes\Eudora.mailto\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\eudora27.exe"" ""%1"""
Root: HKA; Subkey: "Software\Planet Networks\Eudora\Capabilities\URLAssociations"; ValueType: string; ValueName: "mailto"; ValueData: "Eudora.mailto"
Root: HKA; Subkey: "Software\Clients\Mail\Eudora"; ValueType: string; ValueName: ""; ValueData: "Eudora"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Clients\Mail\Eudora\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "Eudora"
Root: HKA; Subkey: "Software\Clients\Mail\Eudora\Capabilities\URLAssociations"; ValueType: string; ValueName: "mailto"; ValueData: "Eudora.mailto"
Root: HKA; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "Eudora"; ValueData: "Software\Planet Networks\Eudora\Capabilities"; Flags: uninsdeletevalue

[InstallDelete]
; development builds' old copies (scripts\install-dev.ps1 renames the one replaced)
Type: files; Name: "{app}\*.old"

[Run]
Filename: "{app}\eudora27.exe"; Description: "Start Eudora"; Flags: nowait postinstall skipifsilent
; an update Eudora started (Restart Now: /SILENT … /relaunch=1; eudora-core/src/update.rs) starts
; Eudora again, as the user who ran it
Filename: "{app}\eudora27.exe"; Flags: nowait runasoriginaluser; Check: Relaunch

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

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Path, Folder: String;
  P: Integer;
begin
  if CurUninstallStep <> usPostUninstall then
    exit;
  if not RegQueryStringValue(EnvRoot, EnvKey, 'Path', Path) then
    exit;
  Folder := ExpandConstant('{app}');
  P := Pos(';' + Uppercase(Folder) + ';', ';' + Uppercase(Path) + ';');
  if P = 0 then
    exit;
  Delete(Path, P - 1, Length(Folder) + 1);
  if (Length(Path) > 0) and (Path[1] = ';') then
    Delete(Path, 1, 1);
  RegWriteExpandStringValue(EnvRoot, EnvKey, 'Path', Path);
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
  AddTip('Everything Eudora does can be scripted: type "eudora --help" in a command prompt. Programs and plugins only get the permissions you grant them.');
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
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    AddToPath(ExpandConstant('{app}'));
end;
