[CmdletBinding()]
param(
    [string]$ToolchainBin,
    [switch]$Offline,
    [switch]$Test,
    [switch]$Cuda,
    [switch]$CpuOnly,
    [switch]$OptimizedSdk,
    [switch]$CloseRunningApp,
    [switch]$NoStage
)

$ErrorActionPreference = 'Stop'
if ($Cuda -and $CpuOnly) { throw 'A CPU-only build cannot use the CUDA runtime.' }
$root = Split-Path $PSScriptRoot -Parent
$sdkRoot = Join-Path $root $(if ($OptimizedSdk) { '.deps/sherpa-pcm-optimized-install' } else { '.deps/sherpa-pcm-install' })
$sdk = Join-Path $sdkRoot 'lib'
$cargoTarget = Join-Path $root 'out/windows-x64-pcm/cargo'
$staging = Join-Path $cargoTarget 'release'
$output = Join-Path $root 'out/windows-x64-pcm/Release'
$runtime = $sdk
$runtimeDlls = @('onnxruntime.dll', 'onnxruntime_providers_shared.dll')
$cudaDll = 'onnxruntime_providers_cuda.dll'
if ($Cuda) {
    $lock = [IO.File]::ReadAllText((Join-Path $root 'cmake/Dependencies.cmake'))
    $archive = [regex]::Match($lock, 'set\(ORT_CUDA_ARCHIVE ([\w.-]+)\.zip\)').Groups[1].Value
    if (!$archive) { throw 'The ONNX Runtime CUDA package pin is missing.' }
    $runtime = Join-Path $root ".deps/onnxruntime/$archive/lib"
    $runtimeDlls += $cudaDll
}
$sherpaDll = 'sherpa-onnx-c-api.dll'
$bins = @('voiceisolator.exe', 'voiceisolator_live.exe', 'voiceisolator_engine.exe')
$oldPath = $env:Path
$oldTarget = $env:CARGO_TARGET_DIR
$oldSdk = $env:VOICEISOLATOR_SHERPA_SDK_DIR

function Copy-Changed {
    param([string]$Source, [string]$Destination)
    if (Test-Path -LiteralPath $Destination) {
        $sha = [Security.Cryptography.SHA256]::Create()
        try {
            $sourceFile = [IO.File]::OpenRead($Source)
            try { $sourceHash = [BitConverter]::ToString($sha.ComputeHash($sourceFile)) }
            finally { $sourceFile.Dispose() }
            $destFile = [IO.File]::OpenRead($Destination)
            try { $destHash = [BitConverter]::ToString($sha.ComputeHash($destFile)) }
            finally { $destFile.Dispose() }
            if ($sourceHash -eq $destHash) { return }
        } finally {
            $sha.Dispose()
        }
    }
    Copy-Item -LiteralPath $Source -Destination $Destination -Force
}

function Copy-Runtime {
    param([string]$Destination)
    Copy-Changed (Join-Path $sdk $sherpaDll) (Join-Path $Destination $sherpaDll)
    foreach ($dll in $runtimeDlls) {
        Copy-Changed (Join-Path $runtime $dll) (Join-Path $Destination $dll)
    }
    $cudaPath = Join-Path $Destination $cudaDll
    if (!$Cuda -and (Test-Path -LiteralPath $cudaPath)) {
        Remove-Item -LiteralPath $cudaPath -Force
    }
}

Push-Location $root
try {
    if ($ToolchainBin) { $env:Path = "$ToolchainBin;$env:Path" }
    $env:CARGO_TARGET_DIR = $cargoTarget
    $env:VOICEISOLATOR_SHERPA_SDK_DIR = $sdkRoot
    if ($OptimizedSdk) {
        $marker = Join-Path $sdkRoot 'voiceisolator-sherpa-patch.txt'
        $sha = [Security.Cryptography.SHA256]::Create()
        try {
            $patch = Join-Path $root 'cmake/sherpa-voxlock.patch'
            $expected = [BitConverter]::ToString($sha.ComputeHash([IO.File]::ReadAllBytes($patch)))
        } finally { $sha.Dispose() }
        if (!(Test-Path -LiteralPath $marker) -or [IO.File]::ReadAllText($marker).Trim() -cne $expected) {
            throw 'The optimized SDK is missing or stale. Run scripts/build-pcm-sdk.ps1 -Optimized.'
        }
    }
    $options = @('--release', '--locked', '-j', '4')
    if ($CpuOnly) { $options += @('--features', 'cpu-only') }
    if ($Offline) { $options += '--offline' }
    if (!(Test-Path -LiteralPath (Join-Path $sdk $sherpaDll))) {
        throw "Missing $sherpaDll. Run scripts/build-pcm-sdk.ps1 first."
    }
    foreach ($dll in $runtimeDlls) {
        if (!(Test-Path -LiteralPath (Join-Path $runtime $dll))) {
            if ($Cuda) { throw "Missing $dll. Run cmake -DVOICEISOLATOR_CUDA=ON -P scripts/fetch-deps.cmake first." }
            throw "Missing $dll. Run scripts/build-pcm-sdk.ps1 first."
        }
    }

    & cargo build @options
    if ($LASTEXITCODE -ne 0) { throw "Rust build failed ($LASTEXITCODE)." }
    Copy-Runtime $staging
    if ($Test) {
        Copy-Runtime (Join-Path $staging 'deps')
        & cargo test @options
        if ($LASTEXITCODE -ne 0) { throw "Rust tests failed ($LASTEXITCODE)." }
        & cargo test @options --lib -- --ignored
        if ($LASTEXITCODE -ne 0) { throw "VoxLock fixture tests failed ($LASTEXITCODE)." }
    }

    if ($NoStage) { return }
    New-Item -ItemType Directory -Force -Path $output | Out-Null
    if ($CloseRunningApp) {
        $outputPath = (Resolve-Path -LiteralPath $output).Path
        $appPaths = @('voiceisolator_live.exe', 'voiceisolator_engine.exe') | ForEach-Object { Join-Path $outputPath $_ }
        $running = @(Get-Process -Name 'voiceisolator_live', 'voiceisolator_engine' -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -in $appPaths })
        $quitEvent = $null
        if ($running.Count -gt 0 -and [Threading.EventWaitHandle]::TryOpenExisting('Local\VoiceIsolator-Quit-v1', [ref]$quitEvent)) {
            try { [void]$quitEvent.Set() } finally { $quitEvent.Dispose() }
        } else {
            foreach ($app in $running) { [void]$app.CloseMainWindow() }
        }
        foreach ($app in $running) {
            if (!$app.WaitForExit(8000)) {
                throw 'Choose Quit from the VoiceIsolator tray menu before rebuilding.'
            }
        }
    }
    Copy-Runtime $output
    foreach ($file in $bins) {
        Copy-Changed (Join-Path $staging $file) (Join-Path $output $file)
    }
    Write-Host "VoiceIsolator is ready in $output"
} finally {
    $env:Path = $oldPath
    $env:CARGO_TARGET_DIR = $oldTarget
    $env:VOICEISOLATOR_SHERPA_SDK_DIR = $oldSdk
    Pop-Location
}
