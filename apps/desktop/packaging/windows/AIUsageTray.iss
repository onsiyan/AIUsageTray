; AI Usage Tray setup, built with Inno Setup 6 by build-installer.ps1, which
; passes the version and the folder holding the release build:
;   ISCC /DAppVersion=0.4.0 /DReleaseDir=...\target\release AIUsageTray.iss
;
; Installs for the current Windows user, with no administrator prompt. An
; update keeps the folder of the install it replaces, and clears what the app
; left there under its earlier name, Usage Monitor.

#ifndef AppVersion
  #error AppVersion must be passed with /DAppVersion=
#endif
#ifndef ReleaseDir
  #error ReleaseDir must be passed with /DReleaseDir=
#endif
#ifndef OutputDir
  #define OutputDir ReleaseDir
#endif

#define AppName "AI Usage Tray"
#define AppExe "ai-usage-tray.exe"
; The name and program the app had before 0.1.0.
#define OldAppName "Usage Monitor"
#define OldAppExe "usage-monitor.exe"
#define AssetDir "..\..\assets\icon"

[Setup]
; Never change: Windows tracks the installed app by this id.
AppId={{6B7C2E1A-4F3D-4C8B-9A61-5E2D8F0B7C34}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisherURL=https://github.com/onsiyan/AIUsageTray
AppSupportURL=https://github.com/onsiyan/AIUsageTray/issues
AppUpdatesURL=https://github.com/onsiyan/AIUsageTray/releases
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
VersionInfoDescription={#AppName} Setup
DefaultDirName={localappdata}\Programs\AIUsageTray
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableWelcomePage=no
DisableReadyPage=no
DisableDirPage=no
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
WizardStyle=modern dynamic
WizardSizePercent=110
SetupIconFile={#AssetDir}\app.ico
WizardImageFile={#AssetDir}\setup-large.png
WizardImageFileDynamicDark={#AssetDir}\setup-large.png
WizardSmallImageFile={#AssetDir}\setup-small.png
WizardSmallImageFileDynamicDark={#AssetDir}\setup-small.png
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
OutputDir={#OutputDir}
OutputBaseFilename=AIUsageTray-{#AppVersion}-Setup
Compression=lzma2/ultra64
SolidCompression=yes
; Close a running copy before its files are replaced, and start it again.
CloseApplications=force
RestartApplications=no
ShowLanguageDialog=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Messages]
WelcomeLabel2=This will install [name/ver] on your computer.%n%nAI Usage Tray sits in the notification area and shows how much of your AI subscriptions you have left: Codex, Claude, Antigravity, and more.%n%nIt is recommended that you close AI Usage Tray before continuing.

[Tasks]
Name: "startup"; Description: "Start AI Usage Tray when I sign in to Windows"; GroupDescription: "Startup:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked

[Files]
Source: "{#ReleaseDir}\ai-usage-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#ReleaseDir}\ai-usage-tray-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#ReleaseDir}\ai-usage-tray-login.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"
Name: "{userdesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"; Tasks: desktopicon

[Registry]
; Starting with Windows is the user's choice here, and a later setup without
; the task clears it.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"""; Flags: uninsdeletevalue; Tasks: startup
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueName: "{#AppName}"; Flags: deletevalue dontcreatekey; Tasks: not startup
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueName: "{#OldAppName}"; Flags: deletevalue dontcreatekey

[Run]
Filename: "{app}\{#AppExe}"; Description: "Open AI Usage Tray now"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#AppExe}"; Flags: runhidden; RunOnceId: "StopApp"

[InstallDelete]
; Left by the earlier IExpress setup, and the programs and shortcuts the app
; had under its earlier name.
Type: files; Name: "{app}\Uninstall.ps1"
Type: files; Name: "{app}\usage-monitor*.exe"
Type: files; Name: "{localappdata}\Programs\UsageMonitor\Uninstall.ps1"
Type: files; Name: "{localappdata}\Programs\UsageMonitor\usage-monitor*.exe"
Type: dirifempty; Name: "{localappdata}\Programs\UsageMonitor"
Type: filesandordirs; Name: "{userprograms}\{#OldAppName}"
Type: files; Name: "{userprograms}\{#OldAppName}.lnk"
Type: files; Name: "{userdesktop}\{#OldAppName}.lnk"

[Code]
// A running copy under the earlier name holds files setup removes; stop it
// first, as CloseApplications does for the current program.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#OldAppExe}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Result := '';
end;

// The earlier IExpress setup registered its own uninstall entry; remove it
// so Windows lists the app once.
procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    RegDeleteKeyIncludingSubkeys(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\UsageMonitor');
end;

// Uninstalling keeps the user's accounts, keys, and settings unless they
// choose to remove them; a silent uninstall keeps them.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ResultCode: Integer;
begin
  if (CurUninstallStep = usUninstall) and not UninstallSilent then
    if MsgBox('Also remove your saved accounts, API keys, and settings from this PC?' + #13#10#13#10 +
        'Choose No to keep them for a later install.', mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
    begin
      Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
      Exec(ExpandConstant('{app}\ai-usage-tray-cli.exe'), 'reset --yes', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    end;
end;
