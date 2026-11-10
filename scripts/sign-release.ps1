[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Minisign,
    [Parameter(Mandatory)][string]$SecretKey,
    [Parameter(Mandatory)][string]$PublicKeyFile
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$output = Join-Path $root 'out/windows-x64-pcm/Release'
$installer = Get-Item -LiteralPath (Join-Path $output 'VoiceIsolator-Setup-x64.exe')
$manifest = Join-Path $output 'VoiceIsolator-release.json'
$version = [regex]::Match((Get-Content -LiteralPath "$root/Cargo.toml" -Raw), '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"').Groups[1].Value
if (!$version -or $installer.VersionInfo.FileVersion.Trim() -ne "$version.0") {
    throw 'The installer version must match the reviewed Cargo.toml version.'
}
if ($installer.Length -gt 256MB -or $installer.Length -le 0) { throw 'The installer size is outside the updater limit.' }
$Minisign = (Resolve-Path -LiteralPath $Minisign).Path
$SecretKey = (Resolve-Path -LiteralPath $SecretKey).Path
$PublicKeyFile = (Resolve-Path -LiteralPath $PublicKeyFile).Path
$build = Get-Content -LiteralPath (Join-Path $output '.installer-build.json') -Raw | ConvertFrom-Json
$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $installer.FullName).Hash
$publicKey = (Get-Content -LiteralPath $PublicKeyFile | Select-Object -Last 1).Trim()
if ($build.sha256 -cne $hash -or $build.publicKey -cne $publicKey) {
    throw 'The installer hash or embedded public key differs from the reviewed build.'
}
$metadata = [ordered]@{
    version = $version
    installer = $installer.Name
    size = $installer.Length
    sha256 = $hash.ToLowerInvariant()
}
$metadata | ConvertTo-Json -Compress | Set-Content -LiteralPath $manifest -Encoding utf8NoBOM
& $Minisign -Sm $manifest -s $SecretKey -t "VoiceIsolator v$version for Windows x64"
if ($LASTEXITCODE) { throw 'Manifest signing failed.' }
& $Minisign -Vm $manifest -p $PublicKeyFile
if ($LASTEXITCODE) { throw 'The signed manifest did not verify with the expected public key.' }
