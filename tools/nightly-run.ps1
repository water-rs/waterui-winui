# Nightly example runner: builds every waterui example against this backend
# and captures a PNG of each running window. Generates one standalone crate
# per example under target\nightly so a single example's build or run failure
# cannot take the others down; crates.io is patched to the checked-out
# waterui workspace so the `App` type unifies between examples and backend.
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $WateruiPath,
    [Parameter(Mandatory)] [string] $OutDir
)

$ErrorActionPreference = 'Stop'

$repo = (Resolve-Path "$PSScriptRoot\..").Path
$WateruiPath = (Resolve-Path $WateruiPath).Path
$OutDir = (New-Item -ItemType Directory -Force -Path $OutDir).FullName
$env:CARGO_TARGET_DIR = Join-Path $repo 'target\nightly\target'
$genRoot = Join-Path $repo 'target\nightly\runners'

# The waterui version this backend pins — the checkout must match it so the
# patch below can satisfy our `=` requirements.
$manifest = Get-Content (Join-Path $repo 'Cargo.toml') -Raw
if ($manifest -notmatch 'waterui\s*=\s*\{[^}]*version\s*=\s*"=([0-9.]+)"') {
    throw 'cannot find the `waterui = "=..."` pin in waterui-winui/Cargo.toml'
}
$pinned = $Matches[1]
if ($manifest -notmatch 'windows-reactor-setup\s*=\s*\{[^}]*version\s*=\s*"([^"]+)"') {
    throw 'cannot find the `windows-reactor-setup` version in waterui-winui/Cargo.toml'
}
$reactorSetup = $Matches[1]

$meta = cargo metadata --format-version 1 --no-deps --manifest-path (Join-Path $WateruiPath 'Cargo.toml') | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed on the waterui workspace' }
$wateruiPkg = $meta.packages | Where-Object { $_.name -eq 'waterui' } | Select-Object -First 1
if (-not $wateruiPkg -or $wateruiPkg.version -ne $pinned) {
    throw "waterui checkout is version $($wateruiPkg.version) but the backend pins =$pinned"
}

# Only crates our dependency graph actually resolves from crates.io need a
# patch; the rest of the waterui workspace stays internal to the examples.
$ourMeta = cargo metadata --locked --format-version 1 --manifest-path (Join-Path $repo 'Cargo.toml') | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed on waterui-winui' }
$registry = @{}
foreach ($p in $ourMeta.packages) {
    if ($p.source -and $p.source.StartsWith('registry')) { $registry[$p.name] = $true }
}
$patchLines = foreach ($p in $meta.packages) {
    if ($registry.ContainsKey($p.name)) {
        '{0} = {{ path = "{1}" }}' -f $p.name, ((Split-Path $p.manifest_path -Parent) -replace '\\', '/')
    }
}
$patchSection = ($patchLines | Sort-Object) -join "`n"

$examplesRoot = Join-Path $WateruiPath 'examples'
$examples = foreach ($p in $meta.packages) {
    $dir = Split-Path $p.manifest_path -Parent
    if ($dir.StartsWith($examplesRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
        $lib = $p.targets | Where-Object { $_.kind -contains 'lib' } | Select-Object -First 1
        if (-not $lib) {
            Write-Host "::warning::skipping $($p.name): no lib target"
            continue
        }
        if (-not (Select-String -Path $lib.src_path -Pattern 'pub fn app' -Quiet)) {
            Write-Host "::warning::skipping $($p.name): no pub fn app entry point"
            continue
        }
        [pscustomobject]@{ Id = (Split-Path $dir -Leaf); Crate = $p.name; Lib = $lib.name; Dir = $dir }
    }
}
if (-not $examples) { throw "no runnable examples discovered under $examplesRoot" }
Write-Host "Discovered $($examples.Count) examples: $($examples.Id -join ', ')"

$repoFwd = $repo -replace '\\', '/'
$results = foreach ($ex in $examples) {
    Write-Host "::group::$($ex.Id)"
    $prefix = Join-Path $OutDir $ex.Id
    $crateDir = Join-Path $genRoot $ex.Id
    New-Item -ItemType Directory -Force -Path (Join-Path $crateDir 'src') | Out-Null

    Set-Content (Join-Path $crateDir 'src\main.rs') @"
fn main() {
    waterui_winui::run_app($($ex.Lib)::app(waterui::env::Environment::new()))
        .expect("example run failed");
}
"@

    # cargo:rustc-link-arg-bins only applies to the package whose build script
    # emits it, so the self-contained manifest must be embedded by the runner
    # crate's own build script — not by waterui-winui's.
    Set-Content (Join-Path $crateDir 'build.rs') @'
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_reactor_setup::as_self_contained();
    }
}
'@

    $exFwd = $ex.Dir -replace '\\', '/'
    Set-Content (Join-Path $crateDir 'Cargo.toml') @"
[package]
name = "runner-$($ex.Id)"
version = "0.0.0"
edition = "2024"

[workspace]

[dependencies]
waterui-winui = { path = "$repoFwd", features = ["self-contained"] }
waterui = "=$pinned"
$($ex.Crate) = { path = "$exFwd" }

[build-dependencies]
windows-reactor-setup = "$reactorSetup"

[patch.crates-io]
$patchSection
"@

    $buildLog = "$prefix.build.log"
    cargo build --manifest-path (Join-Path $crateDir 'Cargo.toml') *> $buildLog
    $exe = Join-Path $env:CARGO_TARGET_DIR "debug\runner-$($ex.Id).exe"
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path $exe)) {
        Write-Host "::warning::$($ex.Id): build failed, see $buildLog"
        [pscustomobject]@{ Example = $ex.Id; Result = 'build failed'; Title = '' }
        Write-Host "::endgroup::"
        continue
    }

    try {
        $r = & "$PSScriptRoot\capture-window.ps1" -Exe $exe -OutPrefix $prefix `
            -StdoutLog "$prefix.stdout.log" -StderrLog "$prefix.stderr.log" `
            -WindowTimeoutSec 45 -PaintTimeoutSec 45
        $status = if ($r.Painted) { 'painted' } else { 'window shown, uniform pixels' }
        if (-not $r.Painted) { Write-Host "::warning::$($ex.Id): no painted content detected" }
        [pscustomobject]@{ Example = $ex.Id; Result = $status; Title = $r.Title }
    } catch {
        $msg = "$_" -replace '\s+', ' ' -replace '\|', '/'
        Write-Host "::warning::$($ex.Id): $msg"
        [pscustomobject]@{ Example = $ex.Id; Result = "run failed: $msg"; Title = '' }
    }
    Write-Host "::endgroup::"
}

$lines = @('| Example | Result | Window title |', '|---|---|---|')
foreach ($r in $results) { $lines += "| $($r.Example) | $($r.Result) | $($r.Title) |" }
Set-Content (Join-Path $OutDir 'summary.md') ($lines -join "`n")
if ($env:GITHUB_STEP_SUMMARY) {
    Add-Content $env:GITHUB_STEP_SUMMARY ("## Nightly example screenshots`n`n" + ($lines -join "`n"))
}

$ok = ($results | Where-Object { $_.Result -eq 'painted' }).Count
Write-Host "Done: $ok of $($results.Count) examples painted a window."

# Harness completed — per-example failures are warnings in the summary, not
# step failures. Clear the trailing cargo exit code so the step stays green.
exit 0
