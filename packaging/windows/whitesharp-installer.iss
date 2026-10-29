; WhiteSharp Installer
; Generated for Inno Setup 6.x
;
; Package source (the plug-in's own bundler output):
;   ..\..\vstplugins\whitesharp_vst3\target\bundled
;     WhiteSharp.vst3\Contents\x86_64-win\WhiteSharp.vst3
;     WhiteSharp.clap
;     WhiteSharp.dll                                       (VST2)
;
; Bundle command:
;   cargo whitesharp-vst3 bundle whitesharp_vst3 --release
;
; build-whitesharp-installer.ps1 bundles, compiles this script and uploads the
; result; it overrides MySourceDir and MyAppVersion using /D.
;
; Install targets:
;   VST3:  %CommonProgramFiles%\VST3\WhiteSharp.vst3
;   CLAP:  %CommonProgramFiles%\CLAP\WhiteSharp.clap
;   VST2:  %ProgramFiles%\VstPlugins\WhiteSharp.dll
;
; The uninstaller lives in %ProgramFiles%\Futureboard\WhiteSharp and nowhere
; else: a DAW scans the plug-in folders, and an unins000.exe left beside the
; plug-in would be picked up and reported as a broken plug-in. The directory
; page is disabled so it cannot be pointed at a plug-in folder either.

#define MyAppName "WhiteSharp"
#define MyAppPublisher "Futureboard"
#define MyAppURL "https://futureboard.studio/plugins/whitesharp"
#define MyAppIcon "..\..\packages\shared\app\icons\icon.ico"

#ifndef MySourceDir
#define MySourceDir "..\..\vstplugins\whitesharp_vst3\target\bundled"
#endif

#ifndef MyAppVersion
#define MyAppVersion "0.1.3"
#endif

[Setup]
AppId={{4F64AA5A-9362-47DD-96C1-236DAC5CB4C2}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
VersionInfoVersion={#MyAppVersion}

; Program Files only. Plug-in folders receive the plug-ins, never the
; uninstaller.
DefaultDirName={commonpf64}\Futureboard\WhiteSharp
DisableDirPage=yes
UsePreviousAppDir=no
UninstallFilesDir={app}
DisableProgramGroupPage=yes
CreateAppDir=yes

SetupIconFile={#MyAppIcon}
UninstallDisplayIcon={uninstallexe}
UninstallDisplayName={#MyAppName} {#MyAppVersion}

OutputDir=..\..\target\installer
OutputBaseFilename=WhiteSharp_{#MyAppVersion}-windows

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
Source: "{#MySourceDir}\WhiteSharp.vst3\*"; DestDir: "{commoncf64}\VST3\WhiteSharp.vst3"; Components: vst3; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#MySourceDir}\WhiteSharp.clap"; DestDir: "{commoncf64}\CLAP"; Components: clap; Flags: ignoreversion
Source: "{#MySourceDir}\WhiteSharp.dll"; DestDir: "{commonpf64}\VstPlugins"; Components: vst2; Flags: ignoreversion

