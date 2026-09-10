; Futureboard Studio Installer
; Generated for Inno Setup 6.x
;
; Package source:
;   ..\..\out\release\community\windows-x64
;
; The entire staged directory tree is copied into the installation directory:
;   - FutureboardNative.exe
;   - helper executables
;   - runtime DLLs
;   - CEF runtime files
;   - ONNX Runtime
;   - Plugins\
;   - locales\
;   - Resources\
;   - any additional runtime files or directories
;
; Recommended package command:
;   cargo xtask package --profile release --edition community --plugin all
;
; build-installer.ps1 may override MySourceDir and MyAppVersion using /D.
;
; Install targets:
;   Per-user:  %LOCALAPPDATA%\Programs\Futureboard Studio\Studio
;   All-users: %ProgramFiles%\Futureboard Studio\Studio

#define MyAppName "Futureboard Studio"
#define MyAppPublisher "Futureboard"
#define MyAppExeName "FutureboardNative.exe"
#define MyAppIcon "..\..\packages\shared\app\icons\icon.ico"

#ifndef MySourceDir
#define MySourceDir "..\..\out\release\community\windows-x64"
#endif

#ifndef MyAppVersion
#define MyAppVersion "2026.9.10-beta1.3"
#endif

#define MyAppUserDir "{localappdata}\Programs\Futureboard Studio\Studio"
#define MyAppMachineDir "{commonpf64}\Futureboard Studio\Studio"

[Setup]
AppId={{9A56EFD0-B65D-4A48-9B0F-F6214A69F001}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}

DefaultDirName={code:GetDefaultDir}
DefaultGroupName=Futureboard Studio\Studio

SetupIconFile={#MyAppIcon}
UninstallDisplayIcon={app}\{#MyAppExeName}

OutputDir=..\..\target\installer
OutputBaseFilename=FutureboardStudioSetup

Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern

ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64

DisableProgramGroupPage=yes
AllowNoIcons=yes

UsePreviousAppDir=yes
UsePreviousSetupType=yes

PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog commandline

CloseApplications=yes
RestartApplications=no
SetupLogging=yes
ChangesAssociations=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; \
    Description: "Create a desktop shortcut"; \
    GroupDescription: "Additional shortcuts:"; \
    Flags: unchecked

Name: "fileassoc_apak"; \
    Description: "Associate .apak packages with APAK Installer"; \
    GroupDescription: "File associations:"; \
    Flags: checkedonce

Name: "fileassoc_fbproj"; \
    Description: "Associate .fbproj projects with {#MyAppName}"; \
    GroupDescription: "File associations:"; \
    Flags: checkedonce

[Files]
; Copy the entire xtask-staged package tree into the application directory.
;
; The wildcard matches all files at the package root.
; recursesubdirs copies every nested directory.
; createallsubdirs preserves empty and nested directory structure where possible.
;
; No extension-specific globbing is required.
; No runtime file needs to be listed manually.
Source: "{#MySourceDir}\*"; \
    DestDir: "{app}"; \
    Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#MyAppName}"; \
    Filename: "{app}\{#MyAppExeName}"; \
    WorkingDir: "{app}"; \
    IconFilename: "{app}\{#MyAppExeName}"

Name: "{group}\APAK Installer"; \
    Filename: "{app}\apakinstaller.exe"; \
    WorkingDir: "{app}"; \
    IconFilename: "{app}\apakinstaller.exe"; \
    Check: FileExists(ExpandConstant('{app}\apakinstaller.exe'))

Name: "{group}\Uninstall {#MyAppName}"; \
    Filename: "{uninstallexe}"

Name: "{autodesktop}\{#MyAppName}"; \
    Filename: "{app}\{#MyAppExeName}"; \
    WorkingDir: "{app}"; \
    IconFilename: "{app}\{#MyAppExeName}"; \
    Tasks: desktopicon

[Registry]
; Associate .apak files with APAK Installer.
;
; HKA resolves to:
;   HKCU for per-user installation
;   HKLM for all-users installation

Root: HKA; \
    Subkey: "Software\Classes\.apak"; \
    ValueType: string; \
    ValueName: ""; \
    ValueData: "Futureboard.APAK"; \
    Flags: uninsdeletevalue; \
    Tasks: fileassoc_apak

Root: HKA; \
    Subkey: "Software\Classes\Futureboard.APAK"; \
    ValueType: string; \
    ValueName: ""; \
    ValueData: "Futureboard Audio Package"; \
    Flags: uninsdeletekey; \
    Tasks: fileassoc_apak

Root: HKA; \
    Subkey: "Software\Classes\Futureboard.APAK\DefaultIcon"; \
    ValueType: string; \
    ValueName: ""; \
    ValueData: "{app}\apakinstaller.exe,0"; \
    Tasks: fileassoc_apak

Root: HKA; \
    Subkey: "Software\Classes\Futureboard.APAK\shell\open\command"; \
    ValueType: string; \
    ValueName: ""; \
    ValueData: """{app}\apakinstaller.exe"" ""%1"""; \
    Tasks: fileassoc_apak

; Associate .fbproj project files with Futureboard Studio. Double-clicking a
; project launches `FutureboardNative.exe "<path>"`; the app routes a project
; argument straight to Studio (`StartupRoute::OpenProject`). Icon index 1 of
; the executable is the document icon embedded from app.rc.
Root: HKA; \
    Subkey: "Software\Classes\.fbproj"; \
    ValueType: string; \
    ValueName: ""; \
    ValueData: "Futureboard.Project"; \
    Flags: uninsdeletevalue; \
    Tasks: fileassoc_fbproj

Root: HKA; \
    Subkey: "Software\Classes\Futureboard.Project"; \
    ValueType: string; \
    ValueName: ""; \
    ValueData: "Futureboard Studio Project"; \
    Flags: uninsdeletekey; \
    Tasks: fileassoc_fbproj

Root: HKA; \
    Subkey: "Software\Classes\Futureboard.Project\DefaultIcon"; \
    ValueType: string; \
    ValueName: ""; \
    ValueData: "{app}\{#MyAppExeName},1"; \
    Tasks: fileassoc_fbproj

Root: HKA; \
    Subkey: "Software\Classes\Futureboard.Project\shell\open\command"; \
    ValueType: string; \
    ValueName: ""; \
    ValueData: """{app}\{#MyAppExeName}"" ""%1"""; \
    Tasks: fileassoc_fbproj

; The fbrd:// URL scheme. The account service hands a desktop sign-in back as
; `fbrd://auth/callback?...`, and a jam invite on a web page is
; `fbrd://jam/j/<code>#<secret>`; both must reach the installed Studio. The app
; also writes the per-user form of these keys when it runs, so a portable copy
; works too — the installer's job is the all-users install, where HKCU is not
; the right hive. No task: a Studio that cannot be signed into is not optional.
Root: HKA; \
    Subkey: "Software\Classes\fbrd"; \
    ValueType: string; \
    ValueName: ""; \
    ValueData: "URL:Futureboard Studio"; \
    Flags: uninsdeletekey

Root: HKA; \
    Subkey: "Software\Classes\fbrd"; \
    ValueType: string; \
    ValueName: "URL Protocol"; \
    ValueData: ""

Root: HKA; \
    Subkey: "Software\Classes\fbrd\DefaultIcon"; \
    ValueType: string; \
    ValueName: ""; \
    ValueData: "{app}\{#MyAppExeName},0"

Root: HKA; \
    Subkey: "Software\Classes\fbrd\shell\open\command"; \
    ValueType: string; \
    ValueName: ""; \
    ValueData: """{app}\{#MyAppExeName}"" ""%1"""

[Run]
Filename: "{app}\{#MyAppExeName}"; \
    Description: "Launch {#MyAppName}"; \
    Flags: nowait postinstall skipifsilent runasoriginaluser

; In-app update: the running application starts this installer silently and
; then quits so its files can be replaced. The entry above cannot bring it
; back, because `skipifsilent` is exactly what stops a silent install from
; launching anything. The updater therefore passes `/RELAUNCH`, and this entry
; reopens the app for that case only.
;
; `runasoriginaluser` is required, not decorative: updating a machine-wide
; install runs setup elevated, and without this the app would be relaunched as
; Administrator.
Filename: "{app}\{#MyAppExeName}"; \
    Flags: nowait runasoriginaluser; \
    Check: ShouldRelaunchAfterUpdate

[UninstallDelete]
Type: filesandordirs; Name: "{app}\logs"

[Code]
function GetDefaultDir(Param: string): string;
begin
  if IsAdminInstallMode then
    Result := ExpandConstant('{#MyAppMachineDir}')
  else
    Result := ExpandConstant('{#MyAppUserDir}');
end;

{ Whether the in-app updater asked for the application to be reopened once a
  silent install finishes.

  `/RELAUNCH` is our own switch, not one of Inno's, so it has to be read off
  the command line by hand — Inno ignores switches it does not recognise. The
  `WizardSilent` guard keeps an interactive install on the postinstall
  checkbox, so a user who unticks "Launch Futureboard Studio" is still obeyed. }
function ShouldRelaunchAfterUpdate(): Boolean;
var
  Index: Integer;
begin
  Result := False;
  if not WizardSilent then
    Exit;
  for Index := 1 to ParamCount do
    if CompareText(ParamStr(Index), '/RELAUNCH') = 0 then
    begin
      Result := True;
      Exit;
    end;
end;

function InitializeSetup(): Boolean;
begin
  Result := True;
end;
