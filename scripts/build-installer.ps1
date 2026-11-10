[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Iscc,
    [Parameter(Mandatory)][string]$VCRuntimeDir,
    [Parameter(Mandatory)][string]$UpdatePublicKey,
    [ValidateSet('sdk', 'sdk-vs2022')][string]$NativeBuild = 'sdk',
    [string]$ToolchainBin,
    [switch]$Offline
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$output = Join-Path $root 'out/windows-x64-pcm/Release'
$cargoOutput = Join-Path $root 'out/windows-x64-pcm/cargo/release'
$sdk = Join-Path $root '.deps/sherpa-pcm-optimized-install/lib'
$Iscc = (Resolve-Path -LiteralPath $Iscc).Path
$VCRuntimeDir = (Resolve-Path -LiteralPath $VCRuntimeDir).Path
if ($VCRuntimeDir -notmatch '\\VC\\Redist\\MSVC\\[^\\]+\\x64\\Microsoft\.VC\d+\.CRT$') {
    throw 'Use the official x64 CRT directory from a licensed Visual Studio installation.'
}
$publicKey = [Convert]::FromBase64String($UpdatePublicKey)
if ($UpdatePublicKey -notmatch '^RW[A-Za-z0-9+/]{54}$' -or $publicKey.Length -ne 42 -or $publicKey[0] -ne 69 -or $publicKey[1] -ne 100) {
    throw 'Provide the single-line Minisign public key. Never pass the private key.'
}
$version = [regex]::Match((Get-Content -LiteralPath "$root/Cargo.toml" -Raw), '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"').Groups[1].Value
if (!$version) { throw 'Cargo.toml must contain a stable release version.' }
$lock = Get-Content -LiteralPath "$root/cmake/Dependencies.cmake" -Raw
$payload = Join-Path $root ('.deps/installer-payload-' + [Guid]::NewGuid().ToString('N'))
$originalKey = $env:VOICEISOLATOR_UPDATE_PUBLIC_KEY
$originalPath = $env:Path

Push-Location $root
try {
    if ($ToolchainBin) { $env:Path = "$ToolchainBin;$env:Path" }
    $env:VOICEISOLATOR_UPDATE_PUBLIC_KEY = $UpdatePublicKey
    & "$PSScriptRoot/build-rust.ps1" -OptimizedSdk -CpuOnly -Test -NoStage -Offline:$Offline

    New-Item -ItemType Directory -Path (Join-Path $payload 'models') -Force | Out-Null
    foreach ($file in @('voiceisolator_live.exe', 'voiceisolator_engine.exe')) {
        Copy-Item -LiteralPath (Join-Path $cargoOutput $file) -Destination $payload
    }
    foreach ($file in @('sherpa-onnx-c-api.dll', 'onnxruntime.dll', 'onnxruntime_providers_shared.dll')) {
        Copy-Item -LiteralPath (Join-Path $sdk $file) -Destination $payload
    }
    foreach ($file in @('vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll')) {
        Copy-Item -LiteralPath (Join-Path $VCRuntimeDir $file) -Destination $payload
    }
    foreach ($model in @{ 'enroll.onnx' = 'ENROLL'; 'chunk_int8.onnx' = 'CHUNK' }.GetEnumerator()) {
        $file = Join-Path $root ".deps/models/voxlock-v2/$($model.Key)"
        $expected = [regex]::Match($lock, "set\(VOXLOCK_V2_$($model.Value)_SHA256 ([a-f0-9]{64})\)").Groups[1].Value
        if (!$expected -or (Get-FileHash -Algorithm SHA256 -LiteralPath $file).Hash -ine $expected) {
            throw "SHA-256 verification failed for $file."
        }
        Copy-Item -LiteralPath $file -Destination (Join-Path $payload 'models')
    }
    Set-Content -LiteralPath (Join-Path $payload 'installed-release') -Value $version -Encoding ascii
    & "$PSScriptRoot/collect-release-notices.ps1" -Destination (Join-Path $payload 'licenses') `
        -NativeBuild (Join-Path $root "out/windows-x64-pcm/$NativeBuild") `
        -InnoDirectory (Split-Path $Iscc -Parent) -Offline:$Offline

    New-Item -ItemType Directory -Force -Path $output | Out-Null
    & $Iscc /Qp "/DAppVersion=$version" "/DPayload=$payload" "/O$output" "$root/installer/VoiceIsolator.iss"
    if ($LASTEXITCODE) { throw "Inno Setup failed ($LASTEXITCODE)." }
    $installer = Join-Path $output 'VoiceIsolator-Setup-x64.exe'
    $sha = (Get-FileHash -Algorithm SHA256 -LiteralPath $installer).Hash
    [ordered]@{ sha256 = $sha; publicKey = $UpdatePublicKey } | ConvertTo-Json -Compress |
        Set-Content -LiteralPath (Join-Path $output '.installer-build.json') -Encoding utf8NoBOM
    Get-FileHash -Algorithm SHA256 -LiteralPath $installer
} finally {
    $env:VOICEISOLATOR_UPDATE_PUBLIC_KEY = $originalKey
    $env:Path = $originalPath
    Pop-Location
    if (Test-Path -LiteralPath $payload) {
        $resolved = (Resolve-Path -LiteralPath $payload).Path
        $parent = (Resolve-Path -LiteralPath (Join-Path $root '.deps')).Path
        if ((Split-Path $resolved -Parent) -ieq $parent -and (Split-Path $resolved -Leaf) -match '^installer-payload-[a-f0-9]{32}$') {
            Remove-Item -LiteralPath $resolved -Recurse -Force
        } else {
            throw "Refusing to remove unexpected staging directory $resolved."
        }
    }
}
