; Inno Setup script for the Windows installer.
;
;   iscc /DAppVersion=0.1.0 /DSourceDir=C:\path\to\files /DOutputDir=C:\out packaging\windows\tome.iss
;
; SourceDir holds tome.exe, tome-cli.exe, README.md and LICENSE.
; Installs for the current user by default (no administrator rights needed);
; the first page offers installing for all users instead.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\target\release"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\dist"
#endif

#define ProgId "io.github.peckage.Tome.File"

[Setup]
; Never change AppId: upgrades and the uninstaller find Tome by it.
AppId={{0CED3934-3315-442A-AFB0-DA22B2F7F8D7}
AppName=Tome
AppVersion={#AppVersion}
AppVerName=Tome {#AppVersion}
AppPublisher=Peckage
AppPublisherURL=https://github.com/Peckage/tome
AppSupportURL=https://github.com/Peckage/tome/issues
AppUpdatesURL=https://github.com/Peckage/tome/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\Tome
DefaultGroupName=Tome
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
LicenseFile=..\..\LICENSE
SetupIconFile=..\..\data\tome.ico
UninstallDisplayIcon={app}\tome.exe
UninstallDisplayName=Tome
OutputDir={#OutputDir}
OutputBaseFilename=tome-v{#AppVersion}-windows-x86_64-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ChangesAssociations=yes
ChangesEnvironment=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "addtopath"; Description: "Add Tome to PATH, to use tome-cli in a terminal"; GroupDescription: "Command line:"

[Files]
Source: "{#SourceDir}\tome.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\tome-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Tome"; Filename: "{app}\tome.exe"
Name: "{autodesktop}\Tome"; Filename: "{app}\tome.exe"; Tasks: desktopicon

[Registry]
; Win+R "tome" starts it.
Root: HKA; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\tome.exe"; ValueType: string; ValueData: "{app}\tome.exe"; Flags: uninsdeletekey

; Tome shows up under "Open with" for these files. It is not made the default app.
Root: HKA; Subkey: "Software\Classes\{#ProgId}"; ValueType: string; ValueData: "Text file (Tome)"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\{#ProgId}\DefaultIcon"; ValueType: string; ValueData: """{app}\tome.exe"",0"
Root: HKA; Subkey: "Software\Classes\{#ProgId}\shell\open\command"; ValueType: string; ValueData: """{app}\tome.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\Applications\tome.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "Tome"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\tome.exe\shell\open\command"; ValueType: string; ValueData: """{app}\tome.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\.txt\OpenWithProgids"; ValueType: string; ValueName: "{#ProgId}"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.log\OpenWithProgids"; ValueType: string; ValueName: "{#ProgId}"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.csv\OpenWithProgids"; ValueType: string; ValueName: "{#ProgId}"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.tsv\OpenWithProgids"; ValueType: string; ValueName: "{#ProgId}"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.xml\OpenWithProgids"; ValueType: string; ValueName: "{#ProgId}"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.json\OpenWithProgids"; ValueType: string; ValueName: "{#ProgId}"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.ndjson\OpenWithProgids"; ValueType: string; ValueName: "{#ProgId}"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.jsonl\OpenWithProgids"; ValueType: string; ValueName: "{#ProgId}"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.yaml\OpenWithProgids"; ValueType: string; ValueName: "{#ProgId}"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.yml\OpenWithProgids"; ValueType: string; ValueName: "{#ProgId}"; ValueData: ""; Flags: uninsdeletevalue

[Run]
Filename: "{app}\tome.exe"; Description: "{cm:LaunchProgram,Tome}"; Flags: nowait postinstall skipifsilent

[Code]
// PATH of the current user, or of the machine for an all-users install.
function EnvRoot: Integer;
begin
  if IsAdminInstallMode then
    Result := HKEY_LOCAL_MACHINE
  else
    Result := HKEY_CURRENT_USER;
end;

function EnvKey: String;
begin
  if IsAdminInstallMode then
    Result := 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
  else
    Result := 'Environment';
end;

procedure AddToPath(Dir: String);
var
  Path: String;
begin
  if not RegQueryStringValue(EnvRoot, EnvKey, 'Path', Path) then
    Path := '';
  if Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Path) + ';') > 0 then
    exit;
  if (Path <> '') and (Path[Length(Path)] <> ';') then
    Path := Path + ';';
  RegWriteExpandStringValue(EnvRoot, EnvKey, 'Path', Path + Dir);
end;

procedure RemoveFromPath(Dir: String);
var
  Path: String;
  P: Integer;
begin
  if not RegQueryStringValue(EnvRoot, EnvKey, 'Path', Path) then
    exit;
  Path := ';' + Path + ';';
  P := Pos(';' + Uppercase(Dir) + ';', Uppercase(Path));
  if P = 0 then
    exit;
  Delete(Path, P, Length(Dir) + 1);
  Path := Copy(Path, 2, Length(Path) - 2);
  RegWriteExpandStringValue(EnvRoot, EnvKey, 'Path', Path);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    AddToPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveFromPath(ExpandConstant('{app}'));
end;
