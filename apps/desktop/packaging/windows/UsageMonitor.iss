; Usage Monitor setup, built with Inno Setup 6 by build-installer.ps1, which
; passes the version and the folder holding the release build:
;   ISCC /DAppVersion=0.1.0 /DReleaseDir=...\target\release UsageMonitor.iss
;
; Installs for the current Windows user, with no administrator prompt, into
; the same folder the earlier IExpress setup used, so an update replaces it.

#ifndef AppVersion
  #error AppVersion must be passed with /DAppVersion=
#endif
#ifndef ReleaseDir
  #error ReleaseDir must be passed with /DReleaseDir=
#endif
#ifndef OutputDir
  #define OutputDir ReleaseDir
#endif

#define AppName "Usage Monitor"
#define AppExe "usage-monitor.exe"
#define AssetDir "..\..\assets\icon"

[Setup]
; Never change: Windows tracks the installed app by this id.
AppId={{6B7C2E1A-4F3D-4C8B-9A61-5E2D8F0B7C34}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
VersionInfoDescription={#AppName} Setup
DefaultDirName={localappdata}\Programs\UsageMonitor
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
OutputBaseFilename=UsageMonitor-{#AppVersion}-Setup
Compression=lzma2/ultra64
SolidCompression=yes
; Close a running copy before its files are replaced, and start it again.
CloseApplications=force
RestartApplications=no
ShowLanguageDialog=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Messages]
WelcomeLabel2=This will install [name/ver] on your computer.%n%nUsage Monitor sits in the notification area and shows how much of your AI subscriptions you have left: Codex, Claude, Antigravity, and more.%n%nIt is recommended that you close Usage Monitor before continuing.

[Tasks]
Name: "startup"; Description: "Start Usage Monitor when I sign in to Windows"; GroupDescription: "Startup:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked

[Files]
Source: "{#ReleaseDir}\usage-monitor.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#ReleaseDir}\usage-monitor-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#ReleaseDir}\usage-monitor-login.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"
Name: "{userdesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"; Tasks: desktopicon

[Registry]
; Starting with Windows is the user's choice here, and a later setup without
; the task clears it.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"""; Flags: uninsdeletevalue; Tasks: startup
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueName: "{#AppName}"; Flags: deletevalue dontcreatekey; Tasks: not startup

[Run]
Filename: "{app}\{#AppExe}"; Description: "Open Usage Monitor now"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#AppExe}"; Flags: runhidden; RunOnceId: "StopApp"

[InstallDelete]
; Left by the earlier IExpress setup.
Type: files; Name: "{app}\Uninstall.ps1"

[Code]
// The earlier IExpress setup registered its own uninstall entry; remove it
// so Windows lists the app once.
procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    RegDeleteKeyIncludingSubkeys(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\UsageMonitor');
end;
