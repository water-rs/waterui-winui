# Nightly example runner: packages every waterui example against this backend
# in release, measures the deployable footprint, then runs the packaged binary
# to capture a PNG, startup latency, and memory. Generates one standalone
# crate per example under target\nightly so a single example's build or run
# failure cannot take the others down; crates.io is patched to the checked-out
# waterui workspace so the `App` type unifies between examples and backend.
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $WateruiPath,
    [Parameter(Mandatory)] [string] $OutDir,
    # Restrict the run to one example id (matrix jobs shard by example).
    [string] $Only,
    # Emit the discovered example list as JSON and stop — the workflow's
    # prepare job uses this to build the shard matrix.
    [switch] $List,
    # Build configuration; nightly measures the packaged artifact, which is a
    # release build — a debug run would report performance no user sees.
    [ValidateSet('debug', 'release')] [string] $Configuration = 'release'
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
        # Examples that link waterui-browser-cef need two things from the
        # runner crate: the `cef-runtime` feature (the workspace disables
        # default features, so the sandbox shim is never compiled and the
        # link fails with LNK2019) and the CEF distribution staged beside the
        # exe (`CefRuntimePaths::packaged` resolves the exe's directory).
        $cefDep = $p.dependencies | Where-Object { $_.name -eq 'waterui-browser-cef' } | Select-Object -First 1
        [pscustomobject]@{
            Id      = (Split-Path $dir -Leaf)
            Crate   = $p.name
            Lib     = $lib.name
            Dir     = $dir
            CefDir  = if ($cefDep -and $cefDep.path) { ($cefDep.path -replace '\\', '/') } else { $null }
        }
    }
}
if (-not $examples) { throw "no runnable examples discovered under $examplesRoot" }
if ($List) {
    # The prepare job pipes this script's success stream to build the shard
    # matrix — a bare output expression, since `[Console]::Out` bypasses the
    # pipeline. The array is forced so a single example still emits `[ "id" ]`,
    # which `fromJSON` needs for a matrix list.
    Write-Output (@($examples | ForEach-Object { $_.Id }) | ConvertTo-Json -Compress)
    exit 0
}
if ($Only) {
    $examples = @($examples | Where-Object { $_.Id -eq $Only })
    if (-not $examples) { throw "example '$Only' not discovered under $examplesRoot" }
}
Write-Host "Discovered $($examples.Count) examples: $($examples.Id -join ', ')"

# windows-reactor-setup stages the self-contained runtime from build scripts,
# but every failure in that path is a silent println! that poisons its shared
# cache (a partial nupkg, a truncated msix, or an empty extract dir is never
# retried). When the staged DLL is missing beside a runner exe, repopulate it
# from the crate's own cache — each level verifies by expanding, and a failed
# expansion rebuilds from the level above (msix <- nupkg <- download).
function Repair-SelfContainedRuntime([string] $Dest) {
    $arch = switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64' { 'x64' } 'ARM64' { 'arm64' } 'X86' { 'x86' }
        default { throw "unsupported arch: $env:PROCESSOR_ARCHITECTURE" }
    }
    $tar = "$env:SystemRoot\System32\tar.exe"
    $curl = "$env:SystemRoot\System32\curl.exe"
    $cache = Join-Path $env:LOCALAPPDATA 'windows-reactor-setup\temp'

    # The package identity comes from whichever cache artifact exists; a
    # poisoned run can leave only one of them behind. Directory names use
    # `<id>-<version>` while nupkgs use `<id>.<version>.nupkg`.
    $pkgDir = Get-ChildItem $cache -Directory -Filter 'Microsoft.WindowsAppSDK.Runtime-*' `
        -ErrorAction SilentlyContinue | Select-Object -First 1
    $nupkgItem = Get-ChildItem $cache -File -Filter 'Microsoft.WindowsAppSDK.Runtime.*.nupkg' `
        -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($pkgDir -and $pkgDir.Name -match '^(?<id>.+)-(?<ver>\d[^-]*)$') {
        $name = $Matches.id; $ver = $Matches.ver
    } elseif ($nupkgItem -and $nupkgItem.BaseName -match '^(?<id>.+?)\.(?<ver>\d+(\.\d+)+)$') {
        $name = $Matches.id; $ver = $Matches.ver
    } else { return $false }
    if (-not $pkgDir) { $pkgDir = New-Item -ItemType Directory -Force -Path (Join-Path $cache "$name-$ver") }

    $msix = Join-Path $pkgDir.FullName "MSIX\win10-$arch\Microsoft.WindowsAppRuntime.2.msix"
    $msixExtract = Join-Path $pkgDir.FullName '.msix_extract'
    $nupkg = Join-Path $cache "$name.$ver.nupkg"

    function Expand-Msix([string] $MsixFile, [string] $ExtractDir) {
        if (Test-Path $ExtractDir) { Get-ChildItem $ExtractDir | Remove-Item -Recurse -Force }
        else { New-Item -ItemType Directory $ExtractDir | Out-Null }
        & $tar -xf $MsixFile -C $ExtractDir
        # A truncated archive can still yield the DLL; require a clean exit so
        # a corrupt member later in the stream does not ship a half-staged
        # runtime beside the exe.
        return ($LASTEXITCODE -eq 0) -and (Test-Path (Join-Path $ExtractDir 'Microsoft.WindowsAppRuntime.dll'))
    }

    $msixFromNupkg = $false
    foreach ($attempt in 0, 1, 2) {
        if ((Test-Path $msix) -and (Expand-Msix $msix $msixExtract)) {
            Copy-Item (Join-Path $msixExtract '*') $Dest -Recurse -Force

            # The crate also deploys the WebView2 projection assembly alongside.
            $wv2 = Get-ChildItem $cache -Directory -Filter 'Microsoft.Web.WebView2-*' |
                Select-Object -First 1
            if ($wv2) {
                $core = Join-Path $wv2.FullName "win-$arch\native_uap\Microsoft.Web.WebView2.Core.dll"
                if (Test-Path $core) { Copy-Item $core $Dest -Force }
            }
            return Test-Path (Join-Path $Dest 'Microsoft.WindowsAppRuntime.dll')
        }

        # The msix is absent or failed to expand — rebuild it from the nupkg.
        # A msix produced by this nupkg already failed once: the nupkg itself
        # is corrupt, so force a fresh download on the next attempt.
        if ($msixFromNupkg) { Remove-Item $nupkg -Force -ErrorAction SilentlyContinue }
        $msixFromNupkg = $false
        if (-not (Test-Path $nupkg)) {
            & $curl -sL -o $nupkg "https://www.nuget.org/api/v2/package/$name/$ver"
            if ($LASTEXITCODE -ne 0 -or -not (Test-Path $nupkg)) {
                Remove-Item $nupkg -Force -ErrorAction SilentlyContinue
                continue
            }
        }
        Get-ChildItem $pkgDir.FullName -ErrorAction SilentlyContinue | Remove-Item -Recurse -Force
        & $tar -xf $nupkg -C $pkgDir.FullName --strip-components=1
        if (Test-Path $msix) { $msixFromNupkg = $true }
        else { Remove-Item $nupkg -Force -ErrorAction SilentlyContinue }
    }
    return $false
}

$repoFwd = $repo -replace '\\', '/'

# Emits one result object, persists it as result-<id>.json for the workflow's
# aggregate job, and returns it for the per-job summary.
function Emit-Result($ex, [string] $Result, [string] $Title, $PaintedMs, $PeakMB, $PrivateMB, $PackageBytes) {
    $r = [pscustomobject]@{
        Example      = $ex.Id
        Result       = $Result
        Title        = $Title
        PaintedMs    = $PaintedMs
        PeakMB       = $PeakMB
        PrivateMB    = $PrivateMB
        PackageBytes = $PackageBytes
    }
    $r | ConvertTo-Json | Set-Content (Join-Path $OutDir "result-$($ex.Id).json")
    return $r
}

$results = foreach ($ex in $examples) {
    Write-Host "::group::$($ex.Id)"
    $packageBytes = $null
    $prefix = Join-Path $OutDir $ex.Id
    $crateDir = Join-Path $genRoot $ex.Id
    New-Item -ItemType Directory -Force -Path (Join-Path $crateDir 'src') | Out-Null

    if ($ex.CefDir) {
        # A Windows CEF application is a DLL: the distribution's
        # `bootstrapc.exe`/`bootstrap.exe` launcher is renamed to the app name,
        # creates the OS-sandbox object, and hands it to the DLL's
        # `RunWinMain`/`RunConsoleMain` export. `bootstrapc` keeps the console
        # the harness captures. CEF subprocesses re-enter through the same
        # pair — `--type` marks those launches and must reach
        # `cef_execute_process` before any WinUI work.
        #
        # The package also keeps a bin target: `cargo:rustc-link-arg-bins`
        # (emitted by `as_self_contained`) is rejected for a package with no
        # bins, and the bin doubles as a runnable unsandboxed entry.
        Set-Content (Join-Path $crateDir 'src\lib.rs') @"
fn main() {
    if std::env::args_os()
        .any(|arg| arg == "--type" || arg.to_string_lossy().starts_with("--type="))
    {
        std::process::exit(waterui_browser_cef::run_packaged_subprocess());
    }
    waterui_winui::run_app(|| $($ex.Lib)::app(waterui::env::Environment::new()))
        .expect("example run failed");
}
waterui_browser_cef::cef_bootstrap_main!(main);
"@
        Set-Content (Join-Path $crateDir 'src\main.rs') @"
fn main() {
    waterui_winui::run_app(|| $($ex.Lib)::app(waterui::env::Environment::new()))
        .expect("example run failed");
}
"@
    } else {
        Set-Content (Join-Path $crateDir 'src\main.rs') @"
fn main() {
    waterui_winui::run_app(|| $($ex.Lib)::app(waterui::env::Environment::new()))
        .expect("example run failed");
}
"@
    }

    # cargo:rustc-link-arg-bins only applies to the package whose build script
    # emits it, so the self-contained manifest must be embedded by the runner
    # crate's own build script — not by waterui-winui's. CEF runners are
    # cdylibs (the exe is the prebuilt launcher): the marker manifest embeds
    # into the DLL via rustc-link-arg-cdylib instead.
    $cdylibManifestArgs = if ($ex.CefDir) {
        @'

        // rustc-link-arg-bins does not apply to a cdylib, so re-emit the
        // manifest as_self_contained just wrote with the cdylib link-arg kind.
        let manifest = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap())
            .join("app.manifest");
        let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
        let target_abi = std::env::var("CARGO_CFG_TARGET_ABI").unwrap_or_default();
        match (target_env.as_str(), target_abi.as_str()) {
            ("msvc", _) => {
                println!("cargo:rustc-link-arg-cdylib=/MANIFEST:EMBED");
                println!("cargo:rustc-link-arg-cdylib=/MANIFESTINPUT:{}", manifest.display());
                // rustc passes /DEBUG unconditionally, so a PDB is written even
                // with debug=0 — and writing one for a DLL importing all of
                // libcef.lib kills mspdbsrv with LNK1201. The capture harness
                // needs no symbols; skip the PDB entirely.
                println!("cargo:rustc-link-arg-cdylib=/DEBUG:NONE");
                println!("cargo:rustc-link-arg-bins=/DEBUG:NONE");
            }
            ("gnu", "llvm") => {
                println!("cargo:rustc-link-arg-cdylib=-Wl,/MANIFEST:EMBED");
                println!("cargo:rustc-link-arg-cdylib=-Wl,/MANIFESTINPUT:{}", manifest.display());
            }
            _ => panic!("unsupported target environment: {target_env}{target_abi}"),
        }
'@
    } else { '' }
    Set-Content (Join-Path $crateDir 'build.rs') @"
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_reactor_setup::as_self_contained();$cdylibManifestArgs
    }
}
"@

    $exFwd = $ex.Dir -replace '\\', '/'
    # Declaring waterui-browser-cef here unifies its features with the
    # example's dep: `cef-runtime` selects the real engine (without it the
    # crate's externs stay undefined and the link fails) and exports the
    # `cef_bootstrap_main!` entry points the cdylib needs.
    $cefDepLine = if ($ex.CefDir) {
        "waterui-browser-cef = { path = `"$($ex.CefDir)`", features = [`"cef-runtime`"] }"
    } else { '' }
    $libSection = if ($ex.CefDir) {
        $libName = "runner_$($ex.Id -replace '-', '_')"
        "`n[lib]`nname = `"$libName`"`ncrate-type = [`"cdylib`"]"
    } else { '' }
    Set-Content (Join-Path $crateDir 'Cargo.toml') @"
[package]
name = "runner-$($ex.Id)"
version = "0.0.0"
edition = "2024"
$libSection
[workspace]

[dependencies]
waterui-winui = { path = "$repoFwd", features = ["self-contained"] }
waterui = "=$pinned"
$($ex.Crate) = { path = "$exFwd" }
$cefDepLine

[build-dependencies]
windows-reactor-setup = "$reactorSetup"

[patch.crates-io]
$patchSection
"@

    $buildLog = "$prefix.build.log"
    $buildArgs = @('build', '--manifest-path', (Join-Path $crateDir 'Cargo.toml'))
    if ($Configuration -eq 'release') { $buildArgs += '--release' }
    cargo @buildArgs *> $buildLog
    $profileDir = Join-Path $env:CARGO_TARGET_DIR $Configuration
    if ($ex.CefDir) {
        # The build product is the application DLL; the exe is assembled from
        # `bootstrapc.exe` during CEF staging below.
        $runnerName = "runner_$($ex.Id -replace '-', '_')"
        $built = Test-Path (Join-Path $profileDir "$runnerName.dll")
        $exe = Join-Path $profileDir "$runnerName.exe"
    } else {
        $exe = Join-Path $profileDir "runner-$($ex.Id).exe"
        $built = Test-Path $exe
    }
    if ($LASTEXITCODE -ne 0 -or -not $built) {
        Write-Host "::warning::$($ex.Id): build failed, see $buildLog"
        Emit-Result $ex 'build failed' '' $null $null $null $null
        Write-Host "::endgroup::"
        continue
    }

    # as_self_contained stages the runtime next to the exe from whichever build
    # script ran first (waterui-winui's as a dependency, or the runner's own).
    # When the staged DLL is absent, capture the build-script stdout and the
    # staging cache listing so the crate's silent println! diagnostics are
    # visible in the artifact, then repair from the same cache.
    $runtimeDir = $profileDir
    if (-not (Test-Path (Join-Path $runtimeDir 'Microsoft.WindowsAppRuntime.dll'))) {
        Get-ChildItem (Join-Path $runtimeDir 'build') -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -like 'runner-*' -or $_.Name -like 'waterui-winui-*' } |
            ForEach-Object {
                $out = Join-Path $_.FullName 'output'
                if (Test-Path $out) { Copy-Item $out "$prefix.$($_.Name).buildscript.log" }
            }
        $cache = Join-Path $env:LOCALAPPDATA 'windows-reactor-setup\temp'
        if (Test-Path $cache) {
            Get-ChildItem $cache -Depth 1 -ErrorAction SilentlyContinue |
                ForEach-Object { "{0} ({1} bytes)" -f $_.FullName.Substring($cache.Length), $_.Length } |
                Set-Content "$prefix.staging-cache.log"
        }
        $repaired = $false
        try { $repaired = Repair-SelfContainedRuntime $runtimeDir }
        catch { Write-Host "::warning::$($ex.Id): runtime repair failed: $_" }
        if (-not $repaired) {
            Write-Host "::warning::$($ex.Id): self-contained runtime not staged beside exe"
            Emit-Result $ex 'run failed: self-contained runtime not staged beside exe' '' $null $null $null $null
            Write-Host "::endgroup::"
            continue
        }
        Write-Host "$($ex.Id): repaired self-contained runtime beside exe"
    }

    $runnerMeta = cargo metadata --locked --format-version 1 --manifest-path (Join-Path $crateDir 'Cargo.toml') | ConvertFrom-Json

    # `CefRuntimePaths::packaged` resolves the exe's own directory as the CEF
    # runtime root: `cef-dll-sys` downloads the distribution into its build
    # output, so stage it beside the runner before launch.
    if ($ex.CefDir) {
        $exeDir = Split-Path $exe -Parent
        $cefRoot = Get-ChildItem (Join-Path $profileDir 'build\cef-dll-sys-*\out\cef_windows_*') -Directory -ErrorAction SilentlyContinue |
            Where-Object { Test-Path (Join-Path $_.FullName 'libcef.dll') } |
            Select-Object -First 1
        if (-not $cefRoot) {
            Write-Host "::warning::$($ex.Id): CEF distribution missing from cef-dll-sys build output"
            Emit-Result $ex 'run failed: CEF distribution not found in cef-dll-sys output' '' $null $null $null $null
            Write-Host "::endgroup::"
            continue
        }
        if (-not (Test-Path (Join-Path $exeDir 'libcef.dll'))) {
            Copy-Item (Join-Path $cefRoot.FullName '*') $exeDir -Recurse -Force
        }
        # The application executable is the console-subsystem launcher renamed
        # to the DLL's basename: it creates the sandbox object, loads
        # `runner_<id>.dll`, and calls `RunConsoleMain`. `bootstrap.exe` is the
        # GUI-subsystem variant and would detach the console the harness
        # captures.
        Copy-Item (Join-Path $exeDir 'bootstrapc.exe') $exe -Force

        # The launcher's documented layout keeps the application manifest
        # beside the exe; bootstrapc.exe already embeds the same one, so this
        # sidecar is inert but conventional. Self-contained WinRT activation is
        # instead handled at runtime: waterui-winui re-activates the manifest
        # embedded in the runner DLL via CreateActCtxW (the process context is
        # fixed at launch from the exe and cannot be amended).
        $cefPkg = $runnerMeta.packages | Where-Object { $_.name -eq 'cef' } | Select-Object -First 1
        if (-not $cefPkg) { throw "cef package missing from the $($ex.Id) runner graph" }
        Copy-Item (Join-Path (Split-Path $cefPkg.manifest_path -Parent) 'src\build_util\win\cef-app.exe.manifest') "$exe.manifest" -Force

        # `CefRuntimePaths::validate` requires the runtime manifest
        # `water package` writes; author the same identity from the resolved
        # `cef-dll-sys` version (`<crate>+<cef version>`).
        $cefSysPkg = $runnerMeta.packages | Where-Object { $_.name -eq 'cef-dll-sys' } | Select-Object -First 1
        $cefVersion = ($cefSysPkg.version -split '\+')[-1]
        $cefArch = switch ($env:PROCESSOR_ARCHITECTURE) {
            'AMD64' { 'x86_64' } 'ARM64' { 'arm64' } 'X86' { 'x86' }
            default { throw "unsupported arch: $env:PROCESSOR_ARCHITECTURE" }
        }
        $cefManifestDir = Join-Path $exeDir 'waterui-browser\cef'
        New-Item -ItemType Directory -Force -Path $cefManifestDir | Out-Null
        [ordered]@{
            engine       = 'cef'
            version      = $cefVersion
            platform     = 'windows'
            architecture = $cefArch
        } | ConvertTo-Json -Compress | Set-Content (Join-Path $cefManifestDir 'runtime.json')
    }

    # Deployable footprint: the executable (plus the app DLL for CEF) and
    # exactly what staging placed beside it — the self-contained runtime
    # allowlist from windows-reactor-setup's runtime.txt, the WebView2
    # projection, and for CEF the whole staged distribution.
    $exeDir = Split-Path $exe -Parent
    $packageBytes = [long](Get-Item $exe).Length
    if ($ex.CefDir) { $packageBytes += [long](Get-Item "$exeDir\$runnerName.dll").Length }
    $setupPkg = $runnerMeta.packages | Where-Object { $_.name -eq 'windows-reactor-setup' } | Select-Object -First 1
    if (-not $setupPkg) { throw 'windows-reactor-setup absent from runner dependency graph' }
    $stagedNames = Get-Content (Join-Path (Split-Path $setupPkg.manifest_path -Parent) 'assets\runtime.txt') |
        ForEach-Object { $_.Trim() } | Where-Object { $_ }
    $packagePaths = @($stagedNames) + 'Microsoft.Web.WebView2.Core.dll'
    if ($ex.CefDir) {
        # Every top-level entry the CEF staging copied out of the dist, plus
        # the launcher manifest and generated runtime identity.
        $packagePaths += Get-ChildItem $cefRoot.FullName | ForEach-Object { $_.Name }
        $packagePaths += (Split-Path "$exe.manifest" -Leaf), 'waterui-browser'
    }
    foreach ($name in $packagePaths) {
        $item = Join-Path $exeDir $name
        if (Test-Path $item) {
            $packageBytes += [long]((Get-ChildItem $item -Recurse -File |
                Measure-Object Length -Sum).Sum)
        }
    }

    try {
        $captureArgs = @{}
        if ($ex.CefDir) {
            # CEF reads switches off the process command line — through
            # bootstrapc.exe — so Chromium diagnostics land on stderr, where
            # the harness already captures them.
            $captureArgs.Arguments = @('--enable-logging=stderr', '--v=0')
        }
        $r = & "$PSScriptRoot\capture-window.ps1" -Exe $exe -OutPrefix $prefix `
            -StdoutLog "$prefix.stdout.log" -StderrLog "$prefix.stderr.log" `
            -WindowTimeoutSec 45 -PaintTimeoutSec 45 @captureArgs
        $status = if ($r.Painted) { 'painted' } else { 'window shown, uniform pixels' }
        if (-not $r.Painted) {
            Write-Host "::warning::$($ex.Id): no painted content detected"
            if ($r.Diagnostics) {
                Write-Host "$($ex.Id): process windows: $($r.Diagnostics)"
                $status = "$status ($($r.Diagnostics -replace '\s+', ' ' -replace '\|', '/'))"
            }
        }
        Emit-Result $ex $status $r.Title $r.PaintedMs $r.PeakWorkingSetMB $r.PrivateBytesMB $packageBytes
    } catch {
        $msg = "$_" -replace '\s+', ' ' -replace '\|', '/'
        Write-Host "::warning::$($ex.Id): $msg"
        Emit-Result $ex "run failed: $msg" '' $null $null $null $packageBytes
    }
    Write-Host "::endgroup::"
}

$lines = @('| Example | Result | Package | Window title | First paint | Peak WS |', '|---|---|---|---|---|---|')
foreach ($r in $results) {
    $package = if ($null -ne $r.PackageBytes) { "$('{0:N1}' -f ($r.PackageBytes / 1MB)) MB" } else { '' }
    $startup = if ($null -ne $r.PaintedMs) { "$($r.PaintedMs) ms" } else { '' }
    $peak = if ($null -ne $r.PeakMB) { "$($r.PeakMB) MB" } else { '' }
    $lines += "| $($r.Example) | $($r.Result) | $package | $($r.Title) | $startup | $peak |"
}
Set-Content (Join-Path $OutDir 'summary.md') ($lines -join "`n")
if ($env:GITHUB_STEP_SUMMARY) {
    Add-Content $env:GITHUB_STEP_SUMMARY ("## Nightly example screenshots`n`n" + ($lines -join "`n"))
}

$ok = ($results | Where-Object { $_.Result -eq 'painted' }).Count
Write-Host "Done: $ok of $($results.Count) examples painted a window."

# Harness completed — per-example failures are warnings in the summary, not
# step failures. Clear the trailing cargo exit code so the step stays green.
exit 0
