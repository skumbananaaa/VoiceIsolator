[CmdletBinding()]
param(
    [ValidateSet('Visual Studio 18 2026', 'Visual Studio 17 2022')]
    [string]$Generator = 'Visual Studio 18 2026',
    [switch]$Optimized
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$source = Join-Path $root '.deps/sherpa-pcm-source'
$buildName = if ($Generator -eq 'Visual Studio 17 2022') { 'sherpa-pcm-vs2022' } else { 'sherpa-pcm' }
$build = Join-Path $root "out/$buildName"
$install = Join-Path $root '.deps/sherpa-pcm-install'
$lock = Get-Content (Join-Path $root 'cmake/Dependencies.cmake') -Raw
$pin = [regex]::Match($lock, 'set\(SHERPA_PCM_COMMIT ([0-9a-f]{40})\)').Groups[1].Value
if (!$pin) { throw 'The sherpa PCM source pin is missing.' }

function Invoke-Checked {
    param([string]$Program, [string[]]$Arguments)
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Program exited with code $LASTEXITCODE." }
}

if (!(Test-Path (Join-Path $source '.git'))) {
    Invoke-Checked git @('init', $source)
}
$current = & git -C $source rev-parse --verify --quiet HEAD
if (!$current) {
    Invoke-Checked git @('-C', $source, 'fetch', '--depth=1',
        'https://github.com/k2-fsa/sherpa-onnx.git', $pin)
    Invoke-Checked git @('-C', $source, 'checkout', '--detach', 'FETCH_HEAD')
    $current = Invoke-Checked git @('-C', $source, 'rev-parse', 'HEAD')
}
$changes = Invoke-Checked git @('-C', $source, 'status', '--porcelain')
if ($current -ne $pin -or $changes) { throw 'Expected a clean sherpa source checkout at the pinned commit.' }

if ($Optimized) {
    $patch = Join-Path $root 'cmake/sherpa-voxlock.patch'
    $patched = Join-Path $root '.deps/sherpa-pcm-optimized'
    $install = Join-Path $root '.deps/sherpa-pcm-optimized-install'
    $buildName = if ($Generator -eq 'Visual Studio 17 2022') { 'sdk-vs2022' } else { 'sdk' }
    $build = Join-Path $root "out/windows-x64-pcm/$buildName"
    if (!(Test-Path -LiteralPath (Join-Path $patched '.git'))) {
        Invoke-Checked git @('-C', $source, 'worktree', 'add', '--detach', $patched, $pin)
    }
    if ((Invoke-Checked git @('-C', $patched, 'rev-parse', 'HEAD')) -ne $pin) {
        throw 'The optimized worktree must be at the pinned commit.'
    }
    if (!(Invoke-Checked git @('-C', $patched, 'status', '--porcelain'))) {
        Invoke-Checked git @('-C', $patched, 'apply', $patch)
    }
    $diff = (Invoke-Checked git @('-C', $patched, 'diff', '--no-color', '--no-ext-diff', 'HEAD')) -join "`n"
    if ($diff -cne [IO.File]::ReadAllText($patch).Replace("`r`n", "`n").TrimEnd("`n") -or
        (Invoke-Checked git @('-C', $patched, 'ls-files', '--others', '--exclude-standard'))) {
        throw 'The optimized worktree must contain exactly the checked-in SDK patch.'
    }
    $source = $patched
}

Invoke-Checked cmake @('-S', $source, '-B', $build, '-G', $Generator, '-A', 'x64',
    '-DCMAKE_POLICY_VERSION_MINIMUM=3.5', '-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreadedDLL',
    '-DBUILD_SHARED_LIBS=ON', '-DSHERPA_ONNX_ENABLE_C_API=ON', '-DSHERPA_ONNX_ENABLE_BINARY=OFF',
    '-DSHERPA_ONNX_ENABLE_PORTAUDIO=OFF', '-DSHERPA_ONNX_ENABLE_WEBSOCKET=OFF',
    '-DSHERPA_ONNX_ENABLE_TTS=OFF', '-DSHERPA_ONNX_ENABLE_SPEAKER_DIARIZATION=OFF',
    '-DSHERPA_ONNX_BUILD_C_API_EXAMPLES=OFF',
    '-DSHERPA_ONNX_USE_PRE_INSTALLED_ONNXRUNTIME_IF_AVAILABLE=OFF')
Invoke-Checked cmake @('--build', $build, '--config', 'Release', '--parallel', '4', '--', '/nr:false')
Invoke-Checked cmake @('--install', $build, '--config', 'Release', '--prefix', $install)
Set-Content (Join-Path $install 'voiceisolator-sherpa-pin.txt') $pin -Encoding ascii
if ($Optimized) {
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        $hash = [BitConverter]::ToString($sha.ComputeHash([IO.File]::ReadAllBytes($patch)))
        Set-Content (Join-Path $install 'voiceisolator-sherpa-patch.txt') $hash -Encoding ascii
    } finally { $sha.Dispose() }
}
