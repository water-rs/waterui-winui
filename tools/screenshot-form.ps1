# Runs the form example and captures its window to form-window.png (plus
# form-desktop.png for context). Readiness is signaled by the window title and
# by the window region actually painting non-uniform pixels — no fixed sleeps.
$ErrorActionPreference = 'Stop'

$exe = Join-Path $PSScriptRoot '..\target\debug\form.exe'
$proc = Start-Process -FilePath $exe -PassThru

Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class User32 {
    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr handle, out RECT rect);
}
public struct RECT { public int Left, Top, Right, Bottom; }
'@

try {
    $deadline = [DateTime]::UtcNow.AddMinutes(2)
    while ($true) {
        $proc.Refresh()
        if ($proc.HasExited) { throw "form.exe exited before showing a window (code $($proc.ExitCode))" }
        if (-not [string]::IsNullOrEmpty($proc.MainWindowTitle) -and $proc.MainWindowHandle -ne [IntPtr]::Zero) { break }
        if ([DateTime]::UtcNow -gt $deadline) { throw 'form.exe did not show a window within 2 minutes' }
        Start-Sleep -Milliseconds 100
    }

    $rect = New-Object RECT
    [User32]::GetWindowRect($proc.MainWindowHandle, [ref]$rect) | Out-Null
    $width = $rect.Right - $rect.Left
    $height = $rect.Bottom - $rect.Top
    if ($width -le 0 -or $height -le 0) { throw "form window has an empty rect ($width x $height)" }

    $deadline = [DateTime]::UtcNow.AddMinutes(2)
    $bmp = $null
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
        if ($colors.Count -gt 16) { break }
        $proc.Refresh()
        if ($proc.HasExited) { throw 'form.exe exited while waiting for paint' }
        if ([DateTime]::UtcNow -gt $deadline) { throw 'form window never painted content' }
        Start-Sleep -Milliseconds 100
    }
    $bmp.Save((Join-Path (Get-Location) 'form-window.png'), [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()

    $screen = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $desktop = New-Object System.Drawing.Bitmap $screen.Width, $screen.Height
    $graphics = [System.Drawing.Graphics]::FromImage($desktop)
    $graphics.CopyFromScreen($screen.Left, $screen.Top, 0, 0, $desktop.Size)
    $graphics.Dispose()
    $desktop.Save((Join-Path (Get-Location) 'form-desktop.png'), [System.Drawing.Imaging.ImageFormat]::Png)
    $desktop.Dispose()
} finally {
    $proc.Refresh()
    if (-not $proc.HasExited) {
        $proc.CloseMainWindow() | Out-Null
        if (-not $proc.WaitForExit(30000)) { $proc.Kill() }
    }
}
