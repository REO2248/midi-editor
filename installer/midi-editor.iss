; midi-editor installer — Inno Setup 6
;
; Strategy: a per-user install (no UAC prompt, works unsigned/self-signed
; during development) that lays midi-editor.exe plus its sidecar helpers
; (vst3-host-helper.exe / vst3-host-probe.exe / mcp-bridge.exe) under
; %LOCALAPPDATA%\Programs\midi-editor, registers Application Capabilities so
; the app shows up in "Open with" and Settings > Default apps, and takes the
; default .mid/.midi/.smf association only when the user opts in on the tasks
; page. All registry work is HKCU — the machine-wide default is never touched.
;
; Build with scripts\package.ps1 (stages the files, then invokes ISCC).

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\dist\stage"
#endif
#ifndef OutputDir
  #define OutputDir "..\dist"
#endif

#define AppName "midi-editor"
#define AppPublisher "REO2248"
#define AppExeName "midi-editor.exe"

[Setup]
; stable AppId — upgrades reuse it and never fork the install
AppId={{EAB059B4-F2EF-45BB-B831-998A3373B25F}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppCopyright=MIT OR Apache-2.0
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
PrivilegesRequired=lowest
OutputDir={#OutputDir}
OutputBaseFilename={#AppName}-{#AppVersion}-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\{#AppExeName}
; refuse to install/uninstall over a running app or its helpers — the
; processes get a polite close request so nothing is orphaned
CloseApplications=yes
CloseApplicationsFilter={#AppExeName},vst3-host-helper.exe,vst3-host-probe.exe,mcp-bridge.exe
RestartApplications=no
ChangesAssociations=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked
Name: "assocmid"; Description: "&Open .mid/.midi/.smf files with {#AppName}"; GroupDescription: "File associations:"; Flags: unchecked

[Files]
Source: "{#SourceDir}\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\vst3-host-helper.exe"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#SourceDir}\vst3-host-probe.exe"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#SourceDir}\mcp-bridge.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE-MIT"; DestDir: "{app}"
Source: "{#SourceDir}\LICENSE-APACHE"; DestDir: "{app}"
Source: "{#SourceDir}\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"; Flags: skipifsourcedoesntexist

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Registry]
; Application Capabilities (always): the app appears in "Open with…" and in
; Settings > Apps > Default apps without claiming any default.
Root: HKCU; Subkey: "Software\{#AppPublisher}\{#AppName}\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "{#AppName}"
Root: HKCU; Subkey: "Software\{#AppPublisher}\{#AppName}\Capabilities"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "Pure-SMF MIDI file editor"
Root: HKCU; Subkey: "Software\{#AppPublisher}\{#AppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".mid"; ValueData: "{#AppName}.mid"
Root: HKCU; Subkey: "Software\{#AppPublisher}\{#AppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".midi"; ValueData: "{#AppName}.mid"
Root: HKCU; Subkey: "Software\{#AppPublisher}\{#AppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".smf"; ValueData: "{#AppName}.mid"
Root: HKCU; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "{#AppName}"; ValueData: "Software\{#AppPublisher}\{#AppName}\Capabilities"; Flags: uninsdeletevalue

; delete our whole trees on uninstall — Inno only removes what the flags
; ask for, and orphaned ProgId/capability keys are exactly what the clean-
; uninstall criterion forbids
Root: HKCU; Subkey: "Software\{#AppPublisher}\{#AppName}"; ValueType: none; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\{#AppName}.mid"; ValueType: none; Flags: uninsdeletekey

; ProgId and the open verb. The quoted "%1" keeps paths containing spaces
; (or shell metacharacters) a single, literal argv entry.
Root: HKCU; Subkey: "Software\Classes\{#AppName}.mid"; ValueType: string; ValueName: ""; ValueData: "Standard MIDI File"
Root: HKCU; Subkey: "Software\Classes\{#AppName}.mid"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "Standard MIDI File"
Root: HKCU; Subkey: "Software\Classes\{#AppName}.mid\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#AppExeName},0"
Root: HKCU; Subkey: "Software\Classes\{#AppName}.mid\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExeName}"" ""%1"""

; Opt-in *default* association (assocmid task). Written under HKCU so it is a
; per-user choice and cannot hijack another user's or the machine's default.
; uninsdeletevalue keeps uninstall clean even if Explorer's Default Apps UI
; later pointed the extension at this ProgId anyway.
Root: HKCU; Subkey: "Software\Classes\.mid"; ValueType: string; ValueName: ""; ValueData: "{#AppName}.mid"; Tasks: assocmid; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\.midi"; ValueType: string; ValueName: ""; ValueData: "{#AppName}.mid"; Tasks: assocmid; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\.smf"; ValueType: string; ValueName: ""; ValueData: "{#AppName}.mid"; Tasks: assocmid; Flags: uninsdeletevalue

[Code]
// Kill the app and its sidecar helpers before files are touched — both for
// install-over-running-app and uninstall. Restart Manager (CloseApplications)
// asks nicely but a silent install proceeds even when the app ignores it,
// which would leave a locked exe and live helpers behind.
procedure KillAppProcesses;
var
  ResultCode: Integer;
begin
  Exec('taskkill.exe', '/IM midi-editor.exe /IM vst3-host-helper.exe /IM vst3-host-probe.exe /IM mcp-bridge.exe /T', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Sleep(1500);
  Exec('taskkill.exe', '/F /IM midi-editor.exe /IM vst3-host-helper.exe /IM vst3-host-probe.exe /IM mcp-bridge.exe /T', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

// A UserChoice written through the OS defaults flow still points here after
// uninstall; drop it only while it names our ProgId so a newer association
// with a different app is never reset.
procedure DeleteUserChoice;
var
  i: Integer;
  exts: array of String;
  prog: String;
begin
  exts := ['.mid', '.midi', '.smf'];
  for i := 0 to GetArrayLength(exts) - 1 do
  begin
    if RegQueryStringValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\' + exts[i] + '\UserChoice', 'ProgId', prog) then
      if CompareText(prog, '{#AppName}.mid') = 0 then
        RegDeleteKeyIncludingSubkeys(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\' + exts[i] + '\UserChoice');
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
    KillAppProcesses;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    KillAppProcesses;
    DeleteUserChoice;
  end;
end;

[UninstallDelete]
; remove app-owned leftovers: the install dir itself and the global prefs
; (%APPDATA%\midi-editor). Per-song sidecars (<song>.mid.editor.json) live
; next to the user's .mid files and are user data — never touched.
Type: filesandordirs; Name: "{app}"
Type: filesandordirs; Name: "{userappdata}\{#AppName}"
