[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Cli,
    [Parameter(Mandatory)][string]$Models,
    [Parameter(Mandatory)][string]$Enrollment,
    [Parameter(Mandatory)][string]$InputWav,
    [Parameter(Mandatory)][string]$ReportDirectory,
    [ValidateRange(1, 3600)][int]$Seconds = 20,
    [ValidateRange(1, 80)][int[]]$ChunkMs = @(10, 20, 40, 80),
    [ValidateRange(1, 4)][int[]]$Threads = @(1, 2, 4),
    [ValidateSet('cpu', 'cuda')][string[]]$Backend = @('cpu'),
    [ValidateSet('int8', 'int8-full', 'fp32', 'fp16')][string[]]$Precision,
    [string]$OrtConfig
)

$ErrorActionPreference = 'Stop'
$Cli = (Resolve-Path -LiteralPath $Cli).Path
$Models = (Resolve-Path -LiteralPath $Models).Path
$Enrollment = (Resolve-Path -LiteralPath $Enrollment).Path
$InputWav = (Resolve-Path -LiteralPath $InputWav).Path
New-Item -ItemType Directory -Path $ReportDirectory | Out-Null
$extra = @()
if ($OrtConfig) {
    $OrtConfig = (Resolve-Path -LiteralPath $OrtConfig).Path
    $extra = @('--ort-config', $OrtConfig)
}

function File-Sha256([string]$Path) {
    $hash = [Security.Cryptography.SHA256]::Create()
    try {
        $file = [IO.File]::OpenRead($Path)
        try { [BitConverter]::ToString($hash.ComputeHash($file)).Replace('-', '').ToLowerInvariant() }
        finally { $file.Dispose() }
    } finally { $hash.Dispose() }
}

$binDirectory = Split-Path $Cli -Parent
$sherpaDll = Join-Path $binDirectory 'sherpa-onnx-c-api.dll'
$ortDll = Join-Path $binDirectory 'onnxruntime.dll'
@{
    cli_sha256 = File-Sha256 $Cli
    sherpa_sha256 = $(if (Test-Path -LiteralPath $sherpaDll) { File-Sha256 $sherpaDll })
    onnxruntime_sha256 = $(if (Test-Path -LiteralPath $ortDll) { File-Sha256 $ortDll })
    enrollment_sha256 = File-Sha256 $Enrollment
    input_sha256 = File-Sha256 $InputWav
    backends = $Backend
    ort_config_sha256 = $(if ($OrtConfig) { File-Sha256 $OrtConfig })
    os = [Environment]::OSVersion.VersionString
    processor_identifier = $env:PROCESSOR_IDENTIFIER
    paced_seconds = $Seconds
} | ConvertTo-Json | Set-Content (Join-Path $ReportDirectory 'run.json') -Encoding utf8

foreach ($provider in $Backend) {
  $formats = if ($Precision) { $Precision } elseif ($provider -eq 'cuda') { @('fp32') } else { @('int8') }
  foreach ($format in $formats) {
    foreach ($chunk in $ChunkMs) {
        foreach ($count in $Threads) {
            $case = "$provider-$format-${chunk}ms-${count}t"
            $diagnostics = Join-Path $ReportDirectory "$case.stderr.txt"
            try {
                $ErrorActionPreference = 'Continue'
                $lines = & $Cli bench --models $Models --enrollment $Enrollment --input $InputWav `
                    --seconds $Seconds --paced --chunk-ms $chunk --threads $count --backend $provider --precision $format @extra 2> $diagnostics
                $code = $LASTEXITCODE
            } finally { $ErrorActionPreference = 'Stop' }
            if ($code -eq 0) {
                $report = ($lines -join [Environment]::NewLine) | ConvertFrom-Json
                $report | Add-Member -NotePropertyName worker_gate -NotePropertyValue (
                    $report.service_p99_ms -le (0.8 * $chunk) -and $report.deadline_misses -eq 0)
            } else {
                $report = @{ status = 'failed'; exit_code = $code; backend = $provider; precision = $format
                    chunk_ms = $chunk; threads = $count; worker_gate = $false }
            }
            $report | ConvertTo-Json | Set-Content (Join-Path $ReportDirectory "$case.json") -Encoding utf8
            Write-Host "$case  exit=$code  gate=$($report.worker_gate)"
        }
    }
  }
}
