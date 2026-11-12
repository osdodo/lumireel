#ifndef MyAppVersion
  #define MyAppVersion "0.1.0"
#endif
#ifndef MyArch
  #define MyArch "x64"
#endif
#ifndef MySource
  #define MySource "..\..\target\release"
#endif

[Setup]
AppName=LumiReel
AppVersion={#MyAppVersion}
AppPublisher=osdodo
DefaultDirName={autopf}\LumiReel
DefaultGroupName=LumiReel
UninstallDisplayIcon={app}\lumireel.exe
OutputDir=..\..\dist
OutputBaseFilename=lumireel-{#MyAppVersion}-windows-{#MyArch}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed={#MyArch}
ArchitecturesInstallIn64BitMode={#MyArch}

[Files]
Source: "{#MySource}\lumireel.exe"; DestDir: "{app}"; Flags: ignoreversion
; reel sits next to lumireel: the agent terminal puts this directory on PATH.
Source: "{#MySource}\reel.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\LumiReel"; Filename: "{app}\lumireel.exe"
Name: "{autodesktop}\LumiReel"; Filename: "{app}\lumireel.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Run]
Filename: "{app}\lumireel.exe"; Description: "{cm:LaunchProgram,LumiReel}"; Flags: nowait postinstall skipifsilent
