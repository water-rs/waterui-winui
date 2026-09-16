# Nightly merge: combines the per-example `result-<id>.json` files produced by
# sharded nightly-run.ps1 jobs into one summary table and one metrics.json
# array for trend telemetry.
[CmdletBinding()]
param(
    # Directory containing the downloaded per-example artifacts merged flat.
    [Parameter(Mandatory)] [string] $ArtifactsDir,
    [Parameter(Mandatory)] [string] $OutDir
)

$ErrorActionPreference = 'Stop'
$OutDir = (New-Item -ItemType Directory -Force -Path $OutDir).FullName

$results = Get-ChildItem $ArtifactsDir -Recurse -File -Filter 'result-*.json' |
    ForEach-Object { Get-Content $_.FullName -Raw | ConvertFrom-Json } |
    Sort-Object Example

$lines = @('| Example | Result | Package | Window title | First paint | Peak WS |', '|---|---|---|---|---|---|')
foreach ($r in $results) {
    $package = if ($null -ne $r.PackageBytes) { "$('{0:N1}' -f ($r.PackageBytes / 1MB)) MB" } else { '' }
    $startup = if ($null -ne $r.PaintedMs) { "$($r.PaintedMs) ms" } else { '' }
    $peak = if ($null -ne $r.PeakMB) { "$($r.PeakMB) MB" } else { '' }
    $lines += "| $($r.Example) | $($r.Result) | $package | $($r.Title) | $startup | $peak |"
}
Set-Content (Join-Path $OutDir 'summary.md') ($lines -join "`n")

$metrics = [ordered]@{
    run_at   = [DateTime]::UtcNow.ToString('o')
    examples = @($results | ForEach-Object {
        [ordered]@{
            example        = $_.Example
            result         = $_.Result
            package_bytes  = $_.PackageBytes
            painted_ms     = $_.PaintedMs
            peak_ws_mb     = $_.PeakMB
            private_mb     = $_.PrivateMB
        }
    })
}
$metrics | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $OutDir 'metrics.json')

$ok = ($results | Where-Object { $_.Result -eq 'painted' }).Count
Write-Host "Done: $ok of $($results.Count) examples painted a window."
