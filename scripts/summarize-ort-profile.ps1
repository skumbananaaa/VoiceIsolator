[CmdletBinding()]
param([Parameter(Mandatory)][string]$Profile)

$ErrorActionPreference = 'Stop'
$path = (Resolve-Path -LiteralPath $Profile).Path
$events = [IO.File]::ReadAllText($path) | ConvertFrom-Json
$nodes = @($events | Where-Object { $_.cat -eq 'Node' -and $_.args.provider })
if (!$nodes) { throw 'The trace contains no executed ONNX nodes.' }

$placement = foreach ($group in ($nodes | Group-Object { $_.args.provider })) {
    @{
        provider = $group.Name
        distinct_nodes = @($group.Group | Select-Object -ExpandProperty name -Unique).Count
        executions = $group.Count
        traced_us = ($group.Group | Measure-Object -Property dur -Sum).Sum
    }
}
$copies = foreach ($group in ($nodes | Where-Object { $_.args.op_name -like 'Memcpy*' } | Group-Object { $_.args.op_name })) {
    [long]$bytes = 0
    [int]$reported = 0
    foreach ($event in $group.Group) {
        [long]$size = 0
        if ([long]::TryParse([string]$event.args.output_size, [ref]$size)) {
            $bytes += $size
            $reported++
        }
    }
    @{
        operation = $group.Name
        executions = $group.Count
        executions_with_size = $reported
        reported_output_bytes = $bytes
        traced_us = ($group.Group | Measure-Object -Property dur -Sum).Sum
    }
}
@{ placement = @($placement); graph_copy_nodes = @($copies) } | ConvertTo-Json -Depth 5
