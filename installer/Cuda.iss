var
  CudaTermsPage: TInputOptionWizardPage;
  CudaDownloadPage: TDownloadWizardPage;
  CudaExtractionPage: TOutputMarqueeProgressWizardPage;
  InstallCudaRuntime: Boolean;

procedure ExtractCudaArchive(const Name, Members: String);
var
  Tar, Params: String;
  Status: Integer;
begin
  CudaExtractionPage.SetText('Extracting the verified runtime.', Name);
  Tar := ExpandConstant('{sysnative}\tar.exe');
  Params := '-xf "' + ExpandConstant('{tmp}\' + Name) + '" -C "' +
    ExpandConstant('{tmp}\cuda-extracted') + '" ' + Members;
  if not Exec(Tar, Params, '', SW_HIDE, ewWaitUntilTerminated, Status) then
    RaiseException('Could not start Windows tar: ' + SysErrorMessage(Status));
  if Status <> 0 then
    RaiseException(Format('Could not extract %s (exit code %d).', [Name, Status]));
end;

#include Payload + "\cuda-code.iss"

function NeedsCudaRuntime(): Boolean;
begin
  Result := InstallCudaRuntime;
end;

function CanReuseCudaRuntime(): Boolean;
var
  InstalledId: AnsiString;
begin
  Result := LoadStringFromFile(ExpandConstant('{app}\cuda\runtime-id'), InstalledId) and
    (InstalledId = CudaRuntimeId) and InstalledCudaFilesPresent();
end;

procedure OpenCudaTerms(Sender: TObject);
var
  Url: String;
  ErrorCode: Integer;
begin
  if TNewButton(Sender).Tag = 0 then
    Url := 'https://docs.nvidia.com/cuda/archive/12.9.1/eula/index.html'
  else
    Url := 'https://docs.nvidia.com/deeplearning/cudnn/backend/v9.15.1/reference/eula.html';
  if not ShellExec('open', Url, '', '', SW_SHOWNORMAL, ewNoWait, ErrorCode) then
    MsgBox(Url, mbInformation, MB_OK);
end;

procedure AddTermsButton(const Caption: String; Index: Integer);
var
  Button: TNewButton;
begin
  Button := TNewButton.Create(CudaTermsPage);
  Button.Parent := CudaTermsPage.Surface;
  Button.Caption := Caption;
  Button.Tag := Index;
  Button.SetBounds(Index * ScaleX(162), CudaTermsPage.SurfaceHeight - ScaleY(28), ScaleX(150), ScaleY(25));
  Button.OnClick := @OpenCudaTerms;
end;

procedure InitializeCudaWizard();
begin
  CudaTermsPage := CreateInputOptionPage(wpSelectTasks, 'NVIDIA CUDA support',
    'Review NVIDIA''s license agreements.',
    'CUDA requires a compatible NVIDIA GPU and driver. Setup will download the runtime directly from its publishers.',
    False, False);
  CudaTermsPage.Add('I accept the NVIDIA CUDA Toolkit and cuDNN license agreements.');
  AddTermsButton('CUDA Toolkit license', 0);
  AddTermsButton('cuDNN license', 1);
  CudaDownloadPage := CreateDownloadPage('Downloading CUDA support', 'Setup verifies each download before using it.', nil);
  CudaDownloadPage.ShowBaseNameInsteadOfUrl := True;
  CudaExtractionPage := CreateOutputMarqueeProgressPage('Preparing CUDA support', 'Extracting the verified runtime.');
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := (PageID = CudaTermsPage.ID) and (not WizardIsTaskSelected('cuda') or CanReuseCudaRuntime());
end;

function CudaTermsAccepted(): Boolean;
begin
  if WizardSilent() then
    Result := ExpandConstant('{param:ACCEPTCUDATERMS|0}') = '1'
  else
    Result := CudaTermsPage.Values[0];
end;

function NextButtonClick(PageID: Integer): Boolean;
begin
  Result := (PageID <> CudaTermsPage.ID) or CudaTermsAccepted();
  if not Result then
    SuppressibleMsgBox('Accept both NVIDIA licenses to continue, or go back and deselect CUDA support.',
      mbInformation, MB_OK, IDOK);
end;

function PrepareCuda(): String;
begin
  Result := '';
  InstallCudaRuntime := WizardIsTaskSelected('cuda') and not CanReuseCudaRuntime();
  if not InstallCudaRuntime then
    Exit;
  if not CudaTermsAccepted() then begin
    Result := 'Accept the NVIDIA licenses or deselect CUDA support.';
    Exit;
  end;

  try
    if not FileExists(ExpandConstant('{sysnative}\tar.exe')) then
      RaiseException('Windows tar is missing. It is required to extract the CUDA runtime.');
    CudaDownloadPage.Clear();
    AddCudaDownloads();
    CudaDownloadPage.Show();
    try
      CudaDownloadPage.Download();
    finally
      CudaDownloadPage.Hide();
    end;

    if not ForceDirectories(ExpandConstant('{tmp}\cuda-extracted')) then
      RaiseException('Setup could not create its temporary extraction directory.');
    CudaExtractionPage.Show();
    try
      CudaExtractionPage.Animate();
      ExtractCudaArchives();
    finally
      CudaExtractionPage.Hide();
    end;
    if not DownloadedCudaFilesPresent() then
      RaiseException('A verified archive is missing a required file.');
  except
    Result := 'Setup could not prepare CUDA support. To install without it, run Setup with the CUDA option unchecked.' + #13#10 +
      GetExceptionMessage();
    Log(Result);
  end;
end;
