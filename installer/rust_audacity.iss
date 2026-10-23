; Windows installer for Rust Audacity (Inno Setup 6).
; Built by the release workflow:  ISCC.exe /DAppVersion=0.8.0 installer\rust_audacity.iss
; Build the release binaries first (cargo build --release).

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{6F1C2B9E-4D7A-4E8B-9A35-52B7C4E1A0D3}
AppName=Rust Audacity
AppVersion={#AppVersion}
AppVerName=Rust Audacity {#AppVersion}
AppPublisher=Rust Audacity
AppPublisherURL=https://github.com/rholmes69/Rust_audacity
AppSupportURL=https://github.com/rholmes69/Rust_audacity/issues
AppUpdatesURL=https://github.com/rholmes69/Rust_audacity/releases
DefaultDirName={autopf}\Rust Audacity
DefaultGroupName=Rust Audacity
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\rust_audacity.exe
UninstallDisplayName=Rust Audacity
OutputDir=..\dist
OutputBaseFilename=RustAudacity-{#AppVersion}-setup
SetupIconFile=..\assets\rust_audacity.ico
Compression=lzma2/max
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Install for the current user without admin rights, or for everyone (asks).
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ChangesAssociations=yes
WizardStyle=modern
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "rapfiles"; Description: "Open Rust Audacity projects (.rap) with Rust Audacity"; GroupDescription: "File types:"
Name: "audiofiles"; Description: "Add Rust Audacity to ""Open with"" for audio files (WAV, MP3, FLAC, OGG, M4A, AIFF)"; GroupDescription: "File types:"

[Files]
Source: "..\target\release\rust_audacity.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\rust_audacity_mcp.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\rust_audacity_chat.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\MCP.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\mcp\claude_desktop_config.example.json"; DestDir: "{app}\mcp"; Flags: ignoreversion
Source: "..\mcp\chat_with_ollama.bat"; DestDir: "{app}\mcp"; Flags: ignoreversion
Source: "..\assets\rust_audacity.ico"; DestDir: "{app}\assets"; Flags: ignoreversion
Source: "..\assets\rust_audacity_logo.png"; DestDir: "{app}\assets"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Rust Audacity"; Filename: "{app}\rust_audacity.exe"
Name: "{autodesktop}\Rust Audacity"; Filename: "{app}\rust_audacity.exe"; Tasks: desktopicon

[Registry]
; .rap projects open in Rust Audacity.
Root: HKA; Subkey: "Software\Classes\.rap"; ValueType: string; ValueName: ""; ValueData: "RustAudacity.Project"; Flags: uninsdeletevalue; Tasks: rapfiles
Root: HKA; Subkey: "Software\Classes\.rap\OpenWithProgids"; ValueType: string; ValueName: "RustAudacity.Project"; ValueData: ""; Flags: uninsdeletevalue; Tasks: rapfiles
Root: HKA; Subkey: "Software\Classes\RustAudacity.Project"; ValueType: string; ValueName: ""; ValueData: "Rust Audacity Project"; Flags: uninsdeletekey; Tasks: rapfiles
Root: HKA; Subkey: "Software\Classes\RustAudacity.Project\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\rust_audacity.exe,0"; Tasks: rapfiles
Root: HKA; Subkey: "Software\Classes\RustAudacity.Project\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\rust_audacity.exe"" ""%1"""; Tasks: rapfiles

; "Open with ▸ Rust Audacity" for audio files (doesn't change the user's default player).
Root: HKA; Subkey: "Software\Classes\RustAudacity.Audio"; ValueType: string; ValueName: ""; ValueData: "Audio file"; Flags: uninsdeletekey; Tasks: audiofiles
Root: HKA; Subkey: "Software\Classes\RustAudacity.Audio\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\rust_audacity.exe,0"; Tasks: audiofiles
Root: HKA; Subkey: "Software\Classes\RustAudacity.Audio\shell\open"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "Rust Audacity"; Tasks: audiofiles
Root: HKA; Subkey: "Software\Classes\RustAudacity.Audio\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\rust_audacity.exe"" ""%1"""; Tasks: audiofiles
Root: HKA; Subkey: "Software\Classes\Applications\rust_audacity.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "Rust Audacity"; Flags: uninsdeletekey; Tasks: audiofiles
Root: HKA; Subkey: "Software\Classes\Applications\rust_audacity.exe\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\rust_audacity.exe"" ""%1"""; Tasks: audiofiles
Root: HKA; Subkey: "Software\Classes\.wav\OpenWithProgids"; ValueType: string; ValueName: "RustAudacity.Audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: audiofiles
Root: HKA; Subkey: "Software\Classes\.mp3\OpenWithProgids"; ValueType: string; ValueName: "RustAudacity.Audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: audiofiles
Root: HKA; Subkey: "Software\Classes\.flac\OpenWithProgids"; ValueType: string; ValueName: "RustAudacity.Audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: audiofiles
Root: HKA; Subkey: "Software\Classes\.ogg\OpenWithProgids"; ValueType: string; ValueName: "RustAudacity.Audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: audiofiles
Root: HKA; Subkey: "Software\Classes\.m4a\OpenWithProgids"; ValueType: string; ValueName: "RustAudacity.Audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: audiofiles
Root: HKA; Subkey: "Software\Classes\.aif\OpenWithProgids"; ValueType: string; ValueName: "RustAudacity.Audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: audiofiles
Root: HKA; Subkey: "Software\Classes\.aiff\OpenWithProgids"; ValueType: string; ValueName: "RustAudacity.Audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: audiofiles

[Run]
Filename: "{app}\rust_audacity.exe"; Description: "{cm:LaunchProgram,Rust Audacity}"; Flags: nowait postinstall skipifsilent
