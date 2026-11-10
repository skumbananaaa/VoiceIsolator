param(
    [Parameter(Mandatory)][string]$Payload,
    [Parameter(Mandatory)][string]$DependencyLock
)

$ErrorActionPreference = 'Stop'
$lock = Get-Content -LiteralPath $DependencyLock -Raw

function Read-Pin([string]$Name) {
    $value = [regex]::Match($lock, "(?m)^set\($Name ([^\r\n]+)\)").Groups[1].Value
    if (!$value) { throw "Missing CUDA installer pin $Name." }
    return $value
}

$cuda = Read-Pin CUDA_REDIST_BASE
$cudnn = Read-Pin CUDNN_REDIST_BASE
$ort = Read-Pin ORT_CUDA_ARCHIVE
$archives = @(
    @{
        name = Read-Pin CUDART_ARCHIVE; sha = Read-Pin CUDART_SHA256
        base = "$cuda/cuda_cudart/windows-x86_64"
        files = @{ 'bin\cudart64_12.dll' = 'cuda\cudart64_12.dll'; 'LICENSE' = 'cuda\licenses\CUDA-Runtime.txt' }
    },
    @{
        name = Read-Pin CUBLAS_ARCHIVE; sha = Read-Pin CUBLAS_SHA256
        base = "$cuda/libcublas/windows-x86_64"
        files = @{
            'bin\cublas64_12.dll' = 'cuda\cublas64_12.dll'
            'bin\cublasLt64_12.dll' = 'cuda\cublasLt64_12.dll'
            'LICENSE' = 'cuda\licenses\cuBLAS.txt'
        }
    },
    @{
        name = Read-Pin CUDNN_ARCHIVE; sha = Read-Pin CUDNN_SHA256
        base = "$cudnn/cudnn/windows-x86_64"
        files = @{
            'bin\cudnn64_9.dll' = 'cuda\cudnn64_9.dll'
            'bin\cudnn_adv64_9.dll' = 'cuda\cudnn_adv64_9.dll'
            'bin\cudnn_cnn64_9.dll' = 'cuda\cudnn_cnn64_9.dll'
            'bin\cudnn_engines_precompiled64_9.dll' = 'cuda\cudnn_engines_precompiled64_9.dll'
            'bin\cudnn_engines_runtime_compiled64_9.dll' = 'cuda\cudnn_engines_runtime_compiled64_9.dll'
            'bin\cudnn_graph64_9.dll' = 'cuda\cudnn_graph64_9.dll'
            'bin\cudnn_heuristic64_9.dll' = 'cuda\cudnn_heuristic64_9.dll'
            'bin\cudnn_ops64_9.dll' = 'cuda\cudnn_ops64_9.dll'
            'LICENSE' = 'cuda\licenses\cuDNN.txt'
        }
    },
    @{
        name = $ort; sha = Read-Pin ORT_CUDA_SHA256
        base = (Read-Pin ORT_CUDA_URL).Replace('/${ORT_CUDA_ARCHIVE}', '')
        files = @{
            'lib\onnxruntime.dll' = 'cuda\onnxruntime.dll'
            'lib\onnxruntime_providers_shared.dll' = 'cuda\onnxruntime_providers_shared.dll'
            'lib\onnxruntime_providers_cuda.dll' = 'cuda\onnxruntime_providers_cuda.dll'
            'LICENSE' = 'cuda\licenses\ONNX-Runtime.txt'
            'ThirdPartyNotices.txt' = 'cuda\licenses\ONNX-Runtime-ThirdPartyNotices.txt'
        }
    }
)
$model = @{
    name = 'voxlock_chunk.onnx'; sha = Read-Pin VOXLOCK_V2_CHUNK_FP32_SHA256
    base = Read-Pin VOXLOCK_V2_BASE
}
$downloads = @($archives) + $model
foreach ($item in $downloads) {
    if ($item.sha -cnotmatch '^[0-9a-f]{64}$' -or $item.name -cnotmatch '^[A-Za-z0-9_.-]+$' -or
        $item.base -cnotmatch '^https://[A-Za-z0-9_./-]+$') {
        throw "Invalid CUDA installer pin for $($item.name)."
    }
}

# A change to the allowlist or any source hash invalidates the installed runtime.
$files = [ordered]@{}
foreach ($archive in $archives) {
    $directory = [IO.Path]::GetFileNameWithoutExtension($archive.name)
    foreach ($entry in ($archive.files.GetEnumerator() | Sort-Object Key)) {
        $files["{tmp}\cuda-extracted\$directory\$($entry.Key)"] = $entry.Value
    }
}
$files["{tmp}\$($model.name)"] = 'models\chunk.onnx'
$identity = (@($downloads.sha) + @($files.Keys) + @($files.Values)) -join "`n"
$runtimeId = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($identity)))
Set-Content -LiteralPath (Join-Path $Payload 'cuda-runtime-id') -Value $runtimeId -Encoding ascii -NoNewline

$fileEntries = foreach ($entry in $files.GetEnumerator()) {
    $dir = Split-Path -Parent $entry.Value
    $name = Split-Path -Leaf $entry.Value
    'Source: "{0}"; DestDir: "{{app}}\{1}"; DestName: "{2}"; Flags: external ignoreversion; Tasks: cuda; Check: NeedsCudaRuntime' -f $entry.Key, $dir, $name
}
Set-Content -LiteralPath (Join-Path $Payload 'cuda-files.iss') -Value $fileEntries -Encoding ascii

$code = @(
    "const CudaRuntimeId = '$runtimeId';"
    'procedure AddCudaDownloads();'
    'begin'
    foreach ($item in $downloads) {
        "  CudaDownloadPage.Add('$($item.base)/$($item.name)', '$($item.name)', '$($item.sha)');"
    }
    'end;'
    'procedure ExtractCudaArchives();'
    'begin'
    foreach ($archive in $archives) {
        $directory = [IO.Path]::GetFileNameWithoutExtension($archive.name)
        $members = ($archive.files.Keys | Sort-Object | ForEach-Object { '"' + $directory + '/' + $_.Replace('\', '/') + '"' }) -join ' '
        "  ExtractCudaArchive('$($archive.name)', '$members');"
    }
    'end;'
    'function InstalledCudaFilesPresent(): Boolean;'
    'begin'
    "  Result := " + (($files.Values | ForEach-Object { "FileExists(ExpandConstant('{app}\$_'))" }) -join " and`r`n    ") + ';'
    'end;'
    'function DownloadedCudaFilesPresent(): Boolean;'
    'begin'
    "  Result := " + (($files.Keys | ForEach-Object { "FileExists(ExpandConstant('$_'))" }) -join " and`r`n    ") + ';'
    'end;'
)
Set-Content -LiteralPath (Join-Path $Payload 'cuda-code.iss') -Value $code -Encoding ascii
