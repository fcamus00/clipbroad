; Inno Setup script for the Windows installer. Build it with packaging/windows/build.ps1.
; Per-user install: no admin prompt, files in %LOCALAPPDATA%\Programs\clipbroad, and that folder in
; the user's PATH. Login startup reuses `clipbroad service install` / `uninstall`.
; Defines from the command line: Version (1.2.3), Tag (file name) and Exe (path to clipbroad.exe).

#ifndef Tag
  #define Tag "v" + Version
#endif

[Setup]
AppId=fcamus00.clipbroad
AppName=clipbroad
AppVersion={#Version}
AppPublisher=Felipe Camus
AppPublisherURL=https://github.com/fcamus00/clipbroad
DefaultDirName={autopf}\clipbroad
; The service stores the exe path, so it must stay the same across upgrades.
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
ChangesEnvironment=yes
UninstallDisplayIcon={app}\clipbroad.exe
OutputBaseFilename=clipbroad-{#Tag}-windows-x86_64-setup
WizardStyle=modern
SolidCompression=yes

[Tasks]
Name: startup; Description: "Start clipbroad at login"

[Files]
Source: "{#Exe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourcePath}\..\..\LICENSE-APACHE"; DestDir: "{app}"
Source: "{#SourcePath}\..\..\LICENSE-MIT"; DestDir: "{app}"

[Registry]
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
  ValueData: "{olddata};{app}"; Check: NotInPath(ExpandConstant('{app}'))

[Run]
; Writes the Run registry value and starts the daemon.
Filename: "{app}\clipbroad.exe"; Parameters: "service install"; Flags: runhidden; Tasks: startup; \
  StatusMsg: "Starting clipbroad..."

[UninstallRun]
Filename: "{app}\clipbroad.exe"; Parameters: "service uninstall"; Flags: runhidden; \
  RunOnceId: "ServiceUninstall"

[Code]
const
  EnvKey = 'Environment';

function NotInPath(Dir: String): Boolean;
var
  Path: String;
begin
  Result := not RegQueryStringValue(HKCU, EnvKey, 'Path', Path)
    or (Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Path) + ';') = 0);
end;

// On an upgrade, Windows does not let us replace clipbroad.exe while the daemon runs.
// `service uninstall` stops it, and the [Run] entry starts it again if the task is selected.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Exe: String;
  Code: Integer;
begin
  Exe := ExpandConstant('{app}\clipbroad.exe');
  if FileExists(Exe) then
    Exec(Exe, 'service uninstall', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Result := '';
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Path, Dir: String;
  P: Integer;
begin
  if (CurUninstallStep <> usPostUninstall)
    or not RegQueryStringValue(HKCU, EnvKey, 'Path', Path) then
    exit;
  Dir := ExpandConstant('{app}');
  Path := ';' + Path + ';';
  P := Pos(';' + Uppercase(Dir) + ';', Uppercase(Path));
  if P = 0 then
    exit;
  Delete(Path, P, Length(Dir) + 1);
  RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Copy(Path, 2, Length(Path) - 2));
end;
