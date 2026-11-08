; Rodhareist Installer
; Generated for Inno Setup 6.x
;
; Package source (the plug-in's own bundler output):
;   ..\..\vstplugins\rodhareist_vst3\target\bundled
;     Rodhareist.vst3\Contents\x86_64-win\Rodhareist.vst3
;     Rodhareist.clap
;     Rodhareist.dll                                       (VST2)
;
; Bundle command:
;   cargo rodhareist-vst3 bundle rodhareist_vst3 --release
;
; build-rodhareist-installer.ps1 bundles and compiles this script; it overrides
; MySourceDir, MyAppVersion and MyFileVersion using /D.
;
; Install targets:
;   VST3:  %CommonProgramFiles%\VST3\Rodhareist.vst3
;   CLAP:  %CommonProgramFiles%\CLAP\Rodhareist.clap
;   VST2:  %ProgramFiles%\VstPlugins\Rodhareist.dll
;
; The uninstaller lives in %ProgramFiles%\Futureboard\Rodhareist and nowhere
; else: a DAW scans the plug-in folders, and an unins000.exe left beside the
; plug-in would be picked up and reported as a broken plug-in. The directory
; page is disabled so it cannot be pointed at a plug-in folder either.
;
; Not touched: NAM captures and IRs in Documents\Futureboard Studio\Rodhareist
; and user presets under Documents\Futureboard Studio\Audio Plug-ins. They are
; the player's files, shared with Futureboard Studio, and stay on uninstall.

#define MyAppName "Rodhareist"
#define MyAppPublisher "Futureboard Digital Technologies"
#define MyAppURL "https://futureboard.studio/plugins/rodhareist"
#define MyAppIcon "..\..\packages\shared\app\icons\icon.ico"

#ifndef MySourceDir
#define MySourceDir "..\..\vstplugins\rodhareist_vst3\target\bundled"
#endif

#ifndef MyAppVersion
#define MyAppVersion "2026.11.1"
#endif

; Windows file versions are up to four numbers; a pre-release tag such as
; "-beta2.0.0" belongs in MyAppVersion only.
#ifndef MyFileVersion
#define MyFileVersion "2026.11.1"
#endif

[Setup]
AppId={{4EB6C8A6-8F3D-4167-A624-5CDA5F92C1CF}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
VersionInfoVersion={#MyFileVersion}

; Program Files only. Plug-in folders receive the plug-ins, never the
; uninstaller.
DefaultDirName={commonpf64}\Futureboard\Rodhareist
DisableDirPage=yes
UsePreviousAppDir=no
UninstallFilesDir={app}
DisableProgramGroupPage=yes
CreateAppDir=yes

SetupIconFile={#MyAppIcon}
UninstallDisplayIcon={uninstallexe}
UninstallDisplayName={#MyAppName} {#MyAppVersion}

OutputDir=..\..\target\installer
OutputBaseFilename=Rodhareist_{#MyAppVersion}-windows

Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern

ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

; Common Files and Program Files are machine-wide.
PrivilegesRequired=admin

; A DAW holding the plug-in open would block the overwrite.
CloseApplications=yes
RestartApplications=no
SetupLogging=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Types]
Name: "full"; Description: "All plug-in formats"
Name: "custom"; Description: "Choose formats"; Flags: iscustom

[Components]
Name: "vst3"; Description: "VST3  (Common Files\VST3)"; Types: full custom
Name: "clap"; Description: "CLAP  (Common Files\CLAP)"; Types: full custom
Name: "vst2"; Description: "VST2  (Program Files\VstPlugins)"; Types: full custom

[Files]
Source: "{#MySourceDir}\Rodhareist.vst3\*"; DestDir: "{commoncf64}\VST3\Rodhareist.vst3"; Components: vst3; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#MySourceDir}\Rodhareist.clap"; DestDir: "{commoncf64}\CLAP"; Components: clap; Flags: ignoreversion
Source: "{#MySourceDir}\Rodhareist.dll"; DestDir: "{commonpf64}\VstPlugins"; Components: vst2; Flags: ignoreversion
