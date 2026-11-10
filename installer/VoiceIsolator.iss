#ifndef AppVersion
  #error AppVersion is required
#endif
#ifndef Payload
  #error Payload is required
#endif

[Setup]
AppId={{F3713433-2A03-4765-8266-403AC50E34F2}
AppName=VoiceIsolator
AppVersion={#AppVersion}
AppPublisher=VoiceIsolator contributors
AppPublisherURL=https://github.com/skumbananaaa/VoiceIsolator
AppSupportURL=https://github.com/skumbananaaa/VoiceIsolator/issues
AppUpdatesURL=https://github.com/skumbananaaa/VoiceIsolator/releases
DefaultDirName={localappdata}\Programs\VoiceIsolator
DefaultGroupName=VoiceIsolator
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
MinVersion=10.0.19045
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
UninstallDisplayIcon={app}\voiceisolator_live.exe
OutputBaseFilename=VoiceIsolator-Setup-x64
Compression=lzma2/fast
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
VersionInfoVersion={#AppVersion}.0
LicenseFile={#Payload}\licenses\Microsoft-VC-Runtime-License.txt

[Files]
Source: "{#Payload}\voiceisolator_live.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\voiceisolator_engine.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\sherpa-onnx-c-api.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\onnxruntime.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\onnxruntime_providers_shared.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\vcruntime140.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\vcruntime140_1.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\msvcp140.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\models\enroll.onnx"; DestDir: "{app}\models"; Flags: ignoreversion
Source: "{#Payload}\models\chunk_int8.onnx"; DestDir: "{app}\models"; Flags: ignoreversion
Source: "{#Payload}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion recursesubdirs
Source: "{#Payload}\installed-release"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\VoiceIsolator"; Filename: "{app}\voiceisolator_live.exe"; WorkingDir: "{app}"

[Registry]
Root: HKCU; Subkey: "Software\VoiceIsolator"; ValueType: string; ValueName: "InstalledVersion"; ValueData: "{#AppVersion}"; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "VoiceIsolator"; Flags: uninsdeletevalue dontcreatekey

[Run]
Filename: "{app}\voiceisolator_live.exe"; Parameters: "--after-update"; WorkingDir: "{app}"; Flags: nowait skipifsilent; Check: FromUpdate
Filename: "{app}\voiceisolator_live.exe"; Description: "Open VoiceIsolator"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent; Check: not FromUpdate

[Code]
const
  SynchronizeAccess = $00100000;
  ErrorInvalidParameter = 87;

var
  WizardStarted, InstallFinished: Boolean;

function OpenProcess(Access: Cardinal; Inherit: Boolean; ProcessId: Cardinal): THandle;
  external 'OpenProcess@kernel32.dll stdcall';
function OpenEvent(Access: Cardinal; Inherit: Boolean; Name: String): THandle;
  external 'OpenEventW@kernel32.dll stdcall';
function WaitForSingleObject(Handle: THandle; Timeout: Cardinal): Cardinal;
  external 'WaitForSingleObject@kernel32.dll stdcall';
function CloseHandle(Handle: THandle): Boolean;
  external 'CloseHandle@kernel32.dll stdcall';

function UpdateProcessId(): Integer;
begin
  Result := StrToIntDef(ExpandConstant('{param:UPDATEFROMPID|0}'), 0);
end;

function FromUpdate(): Boolean;
begin
  Result := UpdateProcessId() > 0;
end;

function InitializeSetup(): Boolean;
var
  Installed: String;
  PreviousVersion, NewVersion: Int64;
  ProcessHandle: THandle;
begin
  Result := False;
  if RegQueryStringValue(HKCU, 'Software\VoiceIsolator', 'InstalledVersion', Installed) then
    if StrToVersion(Installed + '.0', PreviousVersion) and StrToVersion('{#AppVersion}.0', NewVersion) then
      if ComparePackedVersion(NewVersion, PreviousVersion) < 0 then begin
        SuppressibleMsgBox('A newer version of VoiceIsolator is already installed.', mbError, MB_OK, IDOK);
        Exit;
      end;

  if FromUpdate() then begin
    ProcessHandle := OpenProcess(SynchronizeAccess, False, UpdateProcessId());
    if ProcessHandle = 0 then
      Result := DLLGetLastError = ErrorInvalidParameter
    else begin
      Result := WaitForSingleObject(ProcessHandle, 15000) = 0;
      CloseHandle(ProcessHandle);
    end;
    if not Result then
      SuppressibleMsgBox('VoiceIsolator did not close. Quit it from the tray and try again.', mbError, MB_OK, IDOK);
  end else
    Result := True;
end;

procedure InitializeWizard();
begin
  WizardStarted := True;
end;

procedure CurStepChanged(Step: TSetupStep);
begin
  if Step = ssDone then
    InstallFinished := True;
end;

procedure DeinitializeSetup();
var
  Exe: String;
  Status: Integer;
begin
  if FromUpdate() and WizardStarted and not InstallFinished then begin
    Exe := ExpandConstant('{app}\voiceisolator_live.exe');
    if FileExists(Exe) then
      Exec(Exe, '--after-update', '', SW_SHOWNORMAL, ewNoWait, Status);
  end;
end;

function AppRunning(): Boolean;
var
  Signal: THandle;
begin
  Signal := OpenEvent(SynchronizeAccess, False, 'Local\VoiceIsolator-Open-v1');
  Result := Signal <> 0;
  if Result then
    CloseHandle(Signal);
end;

function StopInstalledApp(): Boolean;
var
  Exe: String;
  Status, Attempt: Integer;
begin
  Result := True;
  Exe := ExpandConstant('{app}\voiceisolator_live.exe');
  if not FileExists(Exe) or not AppRunning() then
    Exit;
  if not Exec(Exe, '--quit', '', SW_HIDE, ewWaitUntilTerminated, Status) or (Status <> 0) then begin
    Result := False;
    Exit;
  end;
  for Attempt := 1 to 50 do begin
    if not AppRunning() then
      Exit;
    Sleep(100);
  end;
  Result := False;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if not StopInstalledApp() then
    Result := 'Quit VoiceIsolator from the tray before installing.';
end;

function InitializeUninstall(): Boolean;
begin
  Result := StopInstalledApp();
  if not Result then
    SuppressibleMsgBox('Quit VoiceIsolator from the tray before uninstalling.', mbError, MB_OK, IDOK);
end;
