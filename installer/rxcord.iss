; Windows installer, built by .github/workflows/release.yml after `cargo build --release`:  iscc /DVersion=x.y.z rxcord.iss
; Per-user install (no admin): the folder stays writable, so rxcord's self-update can swap rxcord.exe.
#ifndef Version
  #define Version "0.0.0"
#endif
#ifndef Source
  #define Source "..\target\release\rxcord.exe"
#endif

[Setup]
AppId={{6C1E2B6A-4F0B-4B8E-9C2D-7A5E3F1D9B40}
AppName=rxcord
AppVersion={#Version}
AppPublisher=RexSAS
AppPublisherURL=https://github.com/RexSAS/rxcord
DefaultDirName={localappdata}\Programs\rxcord
DefaultGroupName=rxcord
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=.
OutputBaseFilename=rxcord-setup-windows-x86_64
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\rxcord.exe
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "{#Source}"; DestDir: "{app}"; DestName: "rxcord.exe"; Flags: ignoreversion

[Icons]
Name: "{group}\rxcord"; Filename: "{app}\rxcord.exe"
Name: "{group}\Uninstall rxcord"; Filename: "{uninstallexe}"
Name: "{autodesktop}\rxcord"; Filename: "{app}\rxcord.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\rxcord.exe"; Description: "{cm:LaunchProgram,rxcord}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; left by self-updates
Type: files; Name: "{app}\rxcord.old"
Type: files; Name: "{app}\rxcord.new"
