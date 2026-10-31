; Inno Setup script for AI Video Upscaler.
; Build the release exe first (cargo build -p upscaler-gui --release), then:
;   "%ProgramFiles(x86)%\Inno Setup 6\ISCC.exe" /DAppVersion=X.Y.Z installer\upscaler.iss
; Optional: put ffmpeg.exe and ffprobe.exe in installer\ffmpeg\ to bundle them.

#ifndef AppVersion
  #error Pass the version: ISCC /DAppVersion=X.Y.Z upscaler.iss
#endif

#define ExePath AddBackslash(SourcePath) + "..\target\release\AIVideoUpscaler.exe"
#if !FileExists(ExePath)
  #error Build the release exe first: cargo build -p upscaler-gui --release
#endif

[Setup]
; Never change the AppId: Windows recognises upgrades by it.
AppId={{5B1C3E52-7A0F-4C8B-9E1D-2F6A8C4D7B10}
AppName=AI Video Upscaler
AppVersion={#AppVersion}
AppVerName=AI Video Upscaler {#AppVersion}
AppPublisherURL=https://github.com/Lonestarrk/ai-video-upscaler
DefaultDirName={autopf}\AI Video Upscaler
DefaultGroupName=AI Video Upscaler
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputBaseFilename=AIVideoUpscaler-Setup-{#AppVersion}
LicenseFile=..\LICENSE
SetupIconFile=..\crates\upscaler-gui\assets\icon.ico
UninstallDisplayIcon={app}\AIVideoUpscaler.exe
WizardStyle=modern
Compression=lzma2
SolidCompression=yes

[Languages]
Name: "swedish"; MessagesFile: "compiler:Languages\Swedish.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#ExePath}"; DestDir: "{app}"; Flags: ignoreversion
Source: "ffmpeg\ffmpeg.exe"; DestDir: "{app}\ffmpeg"; Flags: ignoreversion skipifsourcedoesntexist
Source: "ffmpeg\ffprobe.exe"; DestDir: "{app}\ffmpeg"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{group}\AI Video Upscaler"; Filename: "{app}\AIVideoUpscaler.exe"
Name: "{autodesktop}\AI Video Upscaler"; Filename: "{app}\AIVideoUpscaler.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\AIVideoUpscaler.exe"; Description: "{cm:LaunchProgram,AI Video Upscaler}"; Flags: nowait postinstall skipifsilent
