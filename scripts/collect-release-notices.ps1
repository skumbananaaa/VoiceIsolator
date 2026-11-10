[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Destination,
    [Parameter(Mandatory)][string]$NativeBuild,
    [Parameter(Mandatory)][string]$InnoDirectory,
    [switch]$Offline
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$cache = Join-Path $root '.deps/release-license-cache'
New-Item -ItemType Directory -Path $Destination, $cache -Force | Out-Null
Copy-Item -LiteralPath "$root/LICENSE", "$root/THIRD_PARTY_NOTICES.md", "$root/cmake/Dependencies.cmake", "$root/cmake/sherpa-voxlock.patch" -Destination $Destination

function Copy-LegalFiles {
    param([string]$Source, [string]$Target, [switch]$Recurse)
    $files = @(Get-ChildItem -LiteralPath $Source -File -Recurse:$Recurse | Where-Object {
        $_.Name -match '(?i)^(LICEN[SC]E|COPYING|NOTICE|OFL|UFL|UNLICENSE|THIRDPARTY)' -or $_.Name -match '(?i)-license\.txt$'
    })
    foreach ($file in $files) {
        $relative = [IO.Path]::GetRelativePath($Source, $file.FullName)
        $dest = Join-Path $Target $relative
        New-Item -ItemType Directory -Path (Split-Path $dest -Parent) -Force | Out-Null
        Copy-Item -LiteralPath $file.FullName -Destination $dest
    }
    return $files.Count
}

function Copy-UpstreamLicense {
    param([string]$Repository, [string]$Revision, [string]$Name, [string]$Target)
    $cached = Join-Path $cache "$Revision-$Name"
    if (!(Test-Path -LiteralPath $cached)) {
        if ($Offline) { throw "Missing cached release license for $Repository at $Revision. Run the installer build online once." }
        Invoke-WebRequest -Uri "https://raw.githubusercontent.com/$Repository/$Revision/$Name" -OutFile $cached
    }
    if ((Get-Item -LiteralPath $cached).Length -lt 100) { throw "Invalid license file $cached." }
    New-Item -ItemType Directory -Path $Target -Force | Out-Null
    Copy-Item -LiteralPath $cached -Destination (Join-Path $Target $Name)
}

$native = [ordered]@{
    'sherpa-onnx' = "$root/.deps/sherpa-pcm-optimized"
    'voxlock' = "$root/third_party/voxlock"
    'inno-setup' = $InnoDirectory
}
$nativeDeps = Join-Path $NativeBuild '_deps'
foreach ($name in @('onnxruntime','eigen','json','kaldi_decoder','kaldi_native_fbank','kaldifst','kissfft','openfst','simple-sentencepiece','utf8proc')) {
    $native[$name] = Join-Path $nativeDeps "$name-src"
}
foreach ($name in $native.Keys) {
    if (!(Copy-LegalFiles -Source $native[$name] -Target (Join-Path $Destination $name))) {
        throw "The license files for $name are missing."
    }
}

$eigenLock = Get-Content -LiteralPath "$root/.deps/sherpa-pcm-optimized/cmake/eigen.cmake" -Raw
$eigenUrl = [regex]::Match($eigenLock, 'https://gitlab\.com/libeigen/eigen/-/archive/[^"\s]+').Value
$eigenHash = [regex]::Match($eigenLock, 'SHA256=([a-f0-9]{64})').Groups[1].Value
if (!$eigenUrl -or !$eigenHash) { throw 'Eigen source provenance is missing.' }
@"
The sherpa-onnx DLL includes Eigen code covered by the Mozilla Public License 2.0.
Obtain the corresponding Eigen source from $eigenUrl
SHA-256 $eigenHash
The applicable license texts are in this directory.
"@ | Set-Content -LiteralPath (Join-Path $Destination 'eigen/SOURCE.txt') -Encoding utf8

@'
Microsoft Visual C++ v14 redistributable DLLs are proprietary Microsoft software.
The installer includes msvcp140.dll, vcruntime140.dll, and vcruntime140_1.dll from the licensed Visual Studio redist directory.
https://visualstudio.microsoft.com/license-terms/
https://learn.microsoft.com/en-us/cpp/windows/redistributing-visual-cpp-files
'@ | Set-Content -LiteralPath (Join-Path $Destination 'Microsoft-VC-Runtime.txt') -Encoding utf8

$runtimeLicenseName = 'Visual-C-V14-License-Redistributable_and_Runtime_ENU.docx'
$runtimeLicense = Join-Path $cache $runtimeLicenseName
if (!(Test-Path -LiteralPath $runtimeLicense)) {
    if ($Offline) { throw 'The Microsoft runtime license is missing from the release cache.' }
    Invoke-WebRequest -Uri "https://visualstudio.microsoft.com/wp-content/uploads/2025/10/$runtimeLicenseName" -OutFile $runtimeLicense
}
Copy-Item -LiteralPath $runtimeLicense -Destination $Destination
$docx = [IO.Compression.ZipFile]::OpenRead($runtimeLicense)
try {
    $stream = $docx.GetEntry('word/document.xml').Open()
    try {
        $xml = [xml]::new()
        $xml.Load($stream)
    } finally { $stream.Dispose() }
} finally { $docx.Dispose() }
$paragraphs = $xml.SelectNodes('//*[local-name()="p"]') | ForEach-Object {
    ($_.SelectNodes('.//*[local-name()="t"]') | ForEach-Object InnerText) -join ''
}
if ($paragraphs[0] -ne 'MICROSOFT SOFTWARE LICENSE TERMS') { throw 'Unexpected Visual C++ runtime license format.' }
@('These terms apply to the bundled Microsoft Visual C++ runtime files.',
  'VoiceIsolator itself is licensed under Apache-2.0.', '') + $paragraphs |
    Set-Content -LiteralPath (Join-Path $Destination 'Microsoft-VC-Runtime-License.txt') -Encoding utf8BOM

$cargoArgs = @('metadata','--locked','--features','cpu-only','--filter-platform','x86_64-pc-windows-msvc','--format-version','1')
if ($Offline) { $cargoArgs += '--offline' }
$json = & cargo @cargoArgs
if ($LASTEXITCODE) { throw 'Could not obtain the locked Windows dependency graph.' }
$graph = $json | ConvertFrom-Json
$included = @{}
$graph.resolve.nodes | ForEach-Object { $included[$_.id] = $true }
$rust = @($graph.packages | Where-Object { $included.ContainsKey($_.id) -and $_.source })
$summary = @('Rust dependencies resolved for the Windows x64 CPU build (including build-time crates).', '')
foreach ($pkg in $rust) {
    $source = Split-Path $pkg.manifest_path -Parent
    $target = Join-Path $Destination "rust/$($pkg.name)-$($pkg.version)"
    $summary += "$($pkg.name) $($pkg.version) | $($pkg.license) | $($pkg.repository)"
    $copied = Copy-LegalFiles -Source $source -Target $target -Recurse

    if ($pkg.name -eq 'epaint_default_fonts') {
        foreach ($asset in @('OFL.txt','UFL.txt','Hack-Regular.txt','egui-icons.txt','emoji-icon-font-mit-license.txt')) {
            Copy-Item -LiteralPath (Join-Path $source "fonts/$asset") -Destination (Join-Path $target 'fonts') -Force
        }
    } elseif ($copied) {
        continue
    }

    # Some crates omit root license files when publishing a subdirectory.
    $repo = switch -Regex ($pkg.repository) {
        '^https://github\.com/AccessKit/accesskit/?$' { 'AccessKit/accesskit'; break }
        '^https://github\.com/emilk/egui(?:/|$)' { 'emilk/egui'; break }
        '^https://github\.com/(brendanzab|rust-windowing)/gl-rs/?$' { 'rust-windowing/gl-rs'; break }
        '^https://github\.com/aclysma/profiling/?$' { 'aclysma/profiling'; break }
        default { throw "No audited license source for $($pkg.name) $($pkg.version)." }
    }
    $revision = (Get-Content -LiteralPath (Join-Path $source '.cargo_vcs_info.json') -Raw | ConvertFrom-Json).git.sha1
    if ($repo -eq 'emilk/egui' -and $pkg.version -eq '0.37.0') {
        # The published crates point to the packaging checkout. Use the released source tag for root license texts.
        $revision = '841a328ba2dd8f6996beb7189ebb7fc00e668a2a'
    }
    if ($revision -notmatch '^[a-f0-9]{40}$') { throw "Missing source revision for $($pkg.name)." }
    $files = if ($repo -eq 'rust-windowing/gl-rs') { @('LICENSE') } else { @('LICENSE-APACHE','LICENSE-MIT') }
    foreach ($file in $files) { Copy-UpstreamLicense $repo $revision $file $target }
}
$summary | Set-Content -LiteralPath (Join-Path $Destination 'Rust-dependencies.txt') -Encoding utf8
