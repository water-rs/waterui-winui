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
using System.Runtime.InteropServices;
public static class User32 {
    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr handle, out RECT rect);
}
public struct RECT { public int Left, Top, Right, Bottom; }
'@
}

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
        if ($proc.MainWindowHandle -ne [IntPtr]::Zero) { break }
        if ([DateTime]::UtcNow -gt $deadline) { throw "$Exe did not show a window within ${WindowTimeoutSec}s" }
        Start-Sleep -Milliseconds 100
    }

    $rect = New-Object RECT
    [User32]::GetWindowRect($proc.MainWindowHandle, [ref]$rect) | Out-Null
    $width = $rect.Right - $rect.Left
    $height = $rect.Bottom - $rect.Top
    if ($width -le 0 -or $height -le 0) { throw "window has an empty rect ($width x $height)" }

    $painted = $false
    $bmp = $null
    $deadline = [DateTime]::UtcNow.AddSeconds($PaintTimeoutSec)
    while ($true) {
        if ($null -ne $bmp) { $bmp.Dispose() }
        $bmp = New-Object System.Drawing.Bitmap $width, $height
        $graphics = [System.Drawing.Graphics]::FromImage($bmp)
        $graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, $bmp.Size)
        $graphics.Dispose()

        $colors = @{}
        for ($x = 0; $x -lt $width; $x += 8) {
            for ($y = 0; $y -lt $height; $y += 8) {
                $colors[$bmp.GetPixel($x, $y).ToArgb()] = $true
            }
        }
        if ($colors.Count -gt 16) { $painted = $true; break }
        $proc.Refresh()
        if ($proc.HasExited) { throw "$Exe exited while waiting for paint" }
        if ([DateTime]::UtcNow -gt $deadline) { break }
        Start-Sleep -Milliseconds 100
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

    [pscustomobject]@{
        Title      = $proc.MainWindowTitle
        Painted    = $painted
        WindowPng  = $windowPng
        DesktopPng = $desktopPng
    }
} finally {
    $proc.Refresh()
    if (-not $proc.HasExited) {
        $proc.CloseMainWindow() | Out-Null
        if (-not $proc.WaitForExit(30000)) { $proc.Kill() }
    }
}
