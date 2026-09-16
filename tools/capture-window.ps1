# Launches an executable, waits for its main window to appear and paint
# non-uniform content, captures the window (and optionally the whole virtual
# screen) to PNG, then closes it. Readiness is derived from the real window
# handle and sampled pixels — never from fixed sleeps.
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $Exe,
    [string[]] $Arguments = @(),
    [Parameter(Mandatory)] [string] $OutPrefix,
    [string] $StdoutLog,
    [string] $StderrLog,
    [int] $WindowTimeoutSec = 60,
    [int] $PaintTimeoutSec = 60,
    [switch] $IncludeDesktop
)

$ErrorActionPreference = 'Stop'

$startParams = @{ FilePath = $Exe; PassThru = $true }
if ($Arguments.Count) { $startParams.ArgumentList = $Arguments }
if ($StdoutLog) { $startParams.RedirectStandardOutput = $StdoutLog }
if ($StderrLog) { $startParams.RedirectStandardError = $StderrLog }
$proc = Start-Process @startParams

Add-Type -AssemblyName System.Drawing, System.Windows.Forms
if (-not ('User32' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class User32 {
    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr handle, out RECT rect);
    [DllImport("user32.dll")]
    public static extern bool GetClientRect(IntPtr handle, out RECT rect);
    [DllImport("user32.dll")]
    public static extern bool ClientToScreen(IntPtr handle, ref POINT point);
    [DllImport("user32.dll")]
    public static extern bool SetWindowPos(IntPtr handle, IntPtr insertAfter, int x, int y, int cx, int cy, uint flags);
    public delegate bool EnumWindowsProc(IntPtr handle, IntPtr param);
    [DllImport("user32.dll")]
    public static extern bool EnumWindows(EnumWindowsProc callback, IntPtr param);
    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr handle, out uint processId);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetClassName(IntPtr handle, StringBuilder name, int count);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetWindowText(IntPtr handle, StringBuilder text, int count);
    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr handle);
    public static List<string> WindowsOfProcess(uint processId) {
        var windows = new List<string>();
        EnumWindowsProc callback = (handle, param) => {
            uint owner;
            GetWindowThreadProcessId(handle, out owner);
            if (owner == processId) {
                var cls = new StringBuilder(256);
                GetClassName(handle, cls, cls.Capacity);
                var title = new StringBuilder(512);
                GetWindowText(handle, title, title.Capacity);
                var rect = new RECT();
                GetWindowRect(handle, out rect);
                windows.Add(string.Format("{0} class={1} title=\"{2}\" visible={3} rect={4}x{5}",
                    handle, cls, title, IsWindowVisible(handle),
                    rect.Right - rect.Left, rect.Bottom - rect.Top));
            }
            return true;
        };
        EnumWindows(callback, IntPtr.Zero);
        return windows;
    }
    // The first visible top-level window that is not a Win32 dialog (#32770).
    // A process showing only a MessageBox — e.g. a missing-runtime prompt —
    // has not shown its application window.
    public static IntPtr AppWindowOfProcess(uint processId) {
        IntPtr found = IntPtr.Zero;
        EnumWindowsProc callback = (handle, param) => {
            uint owner;
            GetWindowThreadProcessId(handle, out owner);
            if (owner == processId && IsWindowVisible(handle)) {
                var cls = new StringBuilder(256);
                GetClassName(handle, cls, cls.Capacity);
                if (cls.ToString() != "#32770") {
                    found = handle;
                    return false;
                }
            }
            return true;
        };
        EnumWindows(callback, IntPtr.Zero);
        return found;
    }
}
public struct RECT { public int Left, Top, Right, Bottom; }
public struct POINT { public int X, Y; }
'@
}

$windowShownMs = $null
$paintedMs = $null

try {
    $deadline = [DateTime]::UtcNow.AddSeconds($WindowTimeoutSec)
    while ($true) {
        $proc.Refresh()
        if ($proc.HasExited) {
            $tail = ''
            if ($StderrLog -and (Test-Path $StderrLog)) {
                $tail = (Get-Content $StderrLog -Tail 20) -join ' | '
            }
            throw "$Exe exited before showing a window (code $($proc.ExitCode)). $tail"
        }
        $appWindow = [User32]::AppWindowOfProcess($proc.Id)
        if ($appWindow -ne [IntPtr]::Zero) {
            $windowShownMs = [int]([DateTime]::Now - $proc.StartTime).TotalMilliseconds
            break
        }
        if ([DateTime]::UtcNow -gt $deadline) {
            $windows = ([User32]::WindowsOfProcess($proc.Id)) -join ' | '
            throw "$Exe did not show a window within ${WindowTimeoutSec}s. Process windows: $windows"
        }
        Start-Sleep -Milliseconds 100
    }

    $rect = New-Object RECT
    [User32]::GetWindowRect($appWindow, [ref]$rect) | Out-Null
    $width = $rect.Right - $rect.Left
    $height = $rect.Bottom - $rect.Top
    if ($width -le 0 -or $height -le 0) { throw "window has an empty rect ($width x $height)" }

    # Paint detection samples the client area only: the title bar and borders
    # are drawn by the OS before any app content exists, so including them
    # reports "painted" for a window that is still blank.
    $client = New-Object RECT
    $origin = New-Object POINT
    [User32]::GetClientRect($appWindow, [ref]$client) | Out-Null
    [User32]::ClientToScreen($appWindow, [ref]$origin) | Out-Null
    $sampleX = $origin.X - $rect.Left
    $sampleY = $origin.Y - $rect.Top
    $sampleW = $client.Right - $client.Left
    $sampleH = $client.Bottom - $client.Top
    # Clamp to the captured bitmap bounds.
    $x0 = [Math]::Max(0, $sampleX)
    $y0 = [Math]::Max(0, $sampleY)
    $x1 = [Math]::Min($width, $sampleX + $sampleW)
    $y1 = [Math]::Min($height, $sampleY + $sampleH)
    $hasClient = ($x1 -gt $x0) -and ($y1 -gt $y0)
    if (-not $hasClient) { $x0 = 0; $y0 = 0; $x1 = $width; $y1 = $height }

    $painted = $false
    $bmp = $null
    # CopyFromScreen photographs the window's screen region — whatever is on
    # top. Re-raising our window above all non-topmost windows before every
    # sample keeps an occluding console or terminal from posing as content.
    $swpFlags = 0x0001 -bor 0x0002 -bor 0x0010 -bor 0x0040
    $deadline = [DateTime]::UtcNow.AddSeconds($PaintTimeoutSec)
    while ($true) {
        [User32]::SetWindowPos($appWindow, [IntPtr]::Zero, 0, 0, 0, 0, $swpFlags) | Out-Null
        if ($null -ne $bmp) { $bmp.Dispose() }
        $bmp = New-Object System.Drawing.Bitmap $width, $height
        $graphics = [System.Drawing.Graphics]::FromImage($bmp)
        $graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, $bmp.Size)
        $graphics.Dispose()

        $freq = @{}
        $total = 0
        for ($x = $x0; $x -lt $x1; $x += 8) {
            for ($y = $y0; $y -lt $y1; $y += 8) {
                $argb = $bmp.GetPixel($x, $y).ToArgb()
                $freq[$argb] = $freq[$argb] + 1
                $total++
            }
        }
        # Painted content always contains pixels that visibly deviate from the
        # modal color; a blank window is a single flat field. A color-count
        # threshold alone misses sparse dark content (dim text, a lone button)
        # whose palette is small but which clearly deviates from the field.
        $dominantArgb = 0
        $dominantCount = 0
        foreach ($entry in $freq.GetEnumerator()) {
            if ($entry.Value -gt $dominantCount) {
                $dominantCount = $entry.Value
                $dominantArgb = $entry.Key
            }
        }
        $dr = ($dominantArgb -shr 16) -band 0xFF
        $dg = ($dominantArgb -shr 8) -band 0xFF
        $db = $dominantArgb -band 0xFF
        $deviating = 0
        foreach ($argb in $freq.Keys) {
            if ($argb -eq $dominantArgb) { continue }
            $r = ($argb -shr 16) -band 0xFF
            $g = ($argb -shr 8) -band 0xFF
            $b = $argb -band 0xFF
            if ([Math]::Abs($r - $dr) + [Math]::Abs($g - $dg) + [Math]::Abs($b - $db) -gt 64) {
                $deviating += $freq[$argb]
            }
        }
        if ($deviating -ge 4 -or $freq.Count -gt 16) {
            $painted = $true
            $paintedMs = [int]([DateTime]::Now - $proc.StartTime).TotalMilliseconds
            break
        }
        $proc.Refresh()
        if ($proc.HasExited) { throw "$Exe exited while waiting for paint (code $($proc.ExitCode))" }
        if ([DateTime]::UtcNow -gt $deadline) { break }
        Start-Sleep -Milliseconds 100
    }
    # The first painted sample can predate late lifecycle work (Loaded
    # handlers, deferred selection, mid-flight transitions). Keep sampling
    # until the frame stops changing — two identical captures back to back
    # mean the UI has settled — so the saved PNG shows the finished window.
    # Animated content never goes stable; the deadline bounds the wait.
    if ($painted) {
        $settleDeadline = [DateTime]::UtcNow.AddSeconds(3)
        $lastSignature = $null
        while ($true) {
            [User32]::SetWindowPos($appWindow, [IntPtr]::Zero, 0, 0, 0, 0, $swpFlags) | Out-Null
            $bmp.Dispose()
            $bmp = New-Object System.Drawing.Bitmap $width, $height
            $graphics = [System.Drawing.Graphics]::FromImage($bmp)
            $graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, $bmp.Size)
            $graphics.Dispose()

            $pixels = New-Object System.Collections.Generic.List[int]
            for ($x = $x0; $x -lt $x1; $x += 8) {
                for ($y = $y0; $y -lt $y1; $y += 8) {
                    $pixels.Add($bmp.GetPixel($x, $y).ToArgb())
                }
            }
            $signature = $pixels -join ','
            if ($null -ne $lastSignature -and $signature -eq $lastSignature) { break }
            $lastSignature = $signature
            if ([DateTime]::UtcNow -gt $settleDeadline) { break }
            Start-Sleep -Milliseconds 100
        }
    }
    $windowPng = "$OutPrefix-window.png"
    $bmp.Save($windowPng, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()

    $desktopPng = $null
    if ($IncludeDesktop) {
        $desktopPng = "$OutPrefix-desktop.png"
        $screen = [System.Windows.Forms.SystemInformation]::VirtualScreen
        $desktop = New-Object System.Drawing.Bitmap $screen.Width, $screen.Height
        $graphics = [System.Drawing.Graphics]::FromImage($desktop)
        $graphics.CopyFromScreen($screen.Left, $screen.Top, 0, 0, $desktop.Size)
        $graphics.Dispose()
        $desktop.Save($desktopPng, [System.Drawing.Imaging.ImageFormat]::Png)
        $desktop.Dispose()
    }

    # Process counters are sampled after the frame settles — the OS maintains
    # PeakWorkingSet64 continuously, so the read needs no polling during run.
    # The process may exit mid-read; a stale counter is not worth failing the
    # capture over.
    $peakWorkingSetMB = $null
    $privateBytesMB = $null
    try {
        $proc.Refresh()
        if (-not $proc.HasExited) {
            $peakWorkingSetMB = [Math]::Round($proc.PeakWorkingSet64 / 1MB, 1)
            $privateBytesMB = [Math]::Round($proc.PrivateMemorySize64 / 1MB, 1)
        }
    } catch { }

    [pscustomobject]@{
        Title            = {
            $t = New-Object System.Text.StringBuilder 512
            [User32]::GetWindowText($appWindow, $t, $t.Capacity) | Out-Null
            $t.ToString()
        }.Invoke()
        Painted          = $painted
        WindowPng        = $windowPng
        DesktopPng       = $desktopPng
        WindowShownMs    = $windowShownMs
        PaintedMs        = $paintedMs
        PeakWorkingSetMB = $peakWorkingSetMB
        PrivateBytesMB   = $privateBytesMB
        # On a blank window, list every top-level window the process owns so
        # the summary can distinguish "content never rendered" from
        # "MainWindowHandle picked the wrong window".
        Diagnostics      = if ($painted) { '' } else {
            ([User32]::WindowsOfProcess($proc.Id)) -join ' | '
        }
    }
} finally {
    $proc.Refresh()
    if (-not $proc.HasExited) {
        $proc.CloseMainWindow() | Out-Null
        if (-not $proc.WaitForExit(30000)) { $proc.Kill() }
    }
}
