<#
.SYNOPSIS
  Unattended gate checks for DeskFrame (plan Task 1.5).

.DESCRIPTION
  Builds and starts deskframe.exe, then checks:
    1. frames exist and are visible
    2. a second instance exits straight away
    3. Show Desktop (Win+D) leaves every frame visible on top of the desktop
    4. activating a frame does not raise it above another window
    5. private memory is under 30 MB
    6. (-RestartExplorer) frames survive an Explorer restart, and check 3 still passes
  Prints PASS/FAIL per check, tails deskframe.log and exits 1 on any FAIL.

.EXAMPLE
  pwsh -File tools/gate.ps1
  pwsh -File tools/gate.ps1 -RestartExplorer -KeepRunning
#>
param(
    [switch]$RestartExplorer,
    [switch]$KeepRunning,
    [switch]$NoBuild
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $repo 'target\release\deskframe.exe'
$logFile = Join-Path $repo 'target\release\deskframe.log'
$script:failed = 0

Add-Type -AssemblyName System.Windows.Forms
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public static class GateWin {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }

    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] static extern IntPtr WindowFromPoint(POINT p);
    [DllImport("user32.dll")] static extern IntPtr GetAncestor(IntPtr h, uint flags);
    [DllImport("user32.dll")] static extern IntPtr GetWindow(IntPtr h, uint cmd);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetShellWindow();
    [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr v);

    public static void PerMonitorDpi() { try { SetProcessDpiAwarenessContext(new IntPtr(-4)); } catch { } }

    public static List<IntPtr> Frames(uint pid) {
        var found = new List<IntPtr>();
        EnumWindows(delegate (IntPtr h, IntPtr l) {
            var sb = new StringBuilder(64);
            GetClassName(h, sb, 64);
            uint owner;
            GetWindowThreadProcessId(h, out owner);
            if (owner == pid && sb.ToString() == "DeskFrame.Frame") found.Add(h);
            return true;
        }, IntPtr.Zero);
        return found;
    }

    /// The top-level window that receives a click at the centre of h.
    public static IntPtr RootAtCentre(IntPtr h) {
        RECT r;
        GetWindowRect(h, out r);
        var p = new POINT { X = (r.L + r.R) / 2, Y = (r.T + Math.Min(r.B, r.T + 60)) / 2 };
        return GetAncestor(WindowFromPoint(p), 2 /* GA_ROOT */);
    }

    /// True when a is below b in z-order.
    public static bool IsBelow(IntPtr a, IntPtr b) {
        for (IntPtr h = GetWindow(b, 2 /* GW_HWNDNEXT */); h != IntPtr.Zero; h = GetWindow(h, 2)) {
            if (h == a) return true;
        }
        return false;
    }
}
'@
[GateWin]::PerMonitorDpi()

function Check([string]$name, [bool]$ok, [string]$detail = '') {
    if ($ok) { Write-Host "PASS  $name $detail" -ForegroundColor Green }
    else { Write-Host "FAIL  $name $detail" -ForegroundColor Red; $script:failed++ }
}

function Test-ShowDesktop([System.Diagnostics.Process]$proc, [string]$label) {
    $shell = New-Object -ComObject Shell.Application
    $shell.ToggleDesktop()
    Start-Sleep -Milliseconds 1200
    $frames = [GateWin]::Frames([uint32]$proc.Id)
    $onTop = @($frames | Where-Object { [GateWin]::RootAtCentre($_) -eq $_ }).Count
    $shell.ToggleDesktop()
    Start-Sleep -Milliseconds 1000
    $visible = @($frames | Where-Object { [GateWin]::IsWindowVisible($_) }).Count
    Check "show desktop $label" ($frames.Count -gt 0 -and $onTop -eq $frames.Count) "($onTop/$($frames.Count) frames on top during Win+D)"
    Check "after show desktop $label" ($visible -eq $frames.Count) "($visible/$($frames.Count) still visible)"
}

if (-not $NoBuild) {
    Push-Location $repo
    cargo build --release
    if ($LASTEXITCODE -ne 0) { Pop-Location; throw 'cargo build failed' }
    Pop-Location
}

Get-Process deskframe -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$repo*" } | Stop-Process -Force
Start-Sleep -Milliseconds 300
Remove-Item $logFile -ErrorAction SilentlyContinue
$proc = Start-Process $exe -PassThru
Start-Sleep -Seconds 2

# 1. Frames exist and are visible.
$frames = [GateWin]::Frames([uint32]$proc.Id)
$visible = @($frames | Where-Object { [GateWin]::IsWindowVisible($_) }).Count
Check 'frames visible' ($frames.Count -gt 0 -and $visible -eq $frames.Count) "($visible/$($frames.Count))"

# 2. Single instance.
$second = Start-Process $exe -PassThru
$exited = $second.WaitForExit(3000)
Check 'single instance' ($exited -and $second.ExitCode -eq 0) "(second copy exited: $exited)"

# 3. Show Desktop.
Test-ShowDesktop $proc ''

# 4. Activating a frame must not raise it above an overlapping window.
if ($frames.Count -gt 0) {
    $r = New-Object GateWin+RECT
    [void][GateWin]::GetWindowRect($frames[0], [ref]$r)
    $form = New-Object System.Windows.Forms.Form
    $form.Text = 'DeskFrame gate cover'
    $form.StartPosition = 'Manual'
    $form.Bounds = New-Object System.Drawing.Rectangle ($r.L + 40), ($r.T + 10), 300, 200
    $form.Show()
    [System.Windows.Forms.Application]::DoEvents()
    Start-Sleep -Milliseconds 300
    $belowBefore = [GateWin]::IsBelow($frames[0], $form.Handle)
    [void][GateWin]::SetForegroundWindow($frames[0])
    Start-Sleep -Milliseconds 400
    [System.Windows.Forms.Application]::DoEvents()
    $belowAfter = [GateWin]::IsBelow($frames[0], $form.Handle)
    $form.Close()
    Check 'activation keeps frame below' ($belowBefore -and $belowAfter) "(before: $belowBefore, after: $belowAfter)"
}

# 5. Memory.
$proc.Refresh()
$mb = [math]::Round($proc.PrivateMemorySize64 / 1MB, 1)
Check 'memory under 30 MB' ($mb -lt 30) "($mb MB private)"

# 6. Explorer restart.
if ($RestartExplorer) {
    Stop-Process -Name explorer -Force
    Start-Sleep -Seconds 3
    if ([GateWin]::GetShellWindow() -eq [IntPtr]::Zero) { Start-Process explorer.exe }
    $deadline = (Get-Date).AddSeconds(15)
    while ([GateWin]::GetShellWindow() -eq [IntPtr]::Zero -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
    Start-Sleep -Seconds 3
    $frames = [GateWin]::Frames([uint32]$proc.Id)
    $visible = @($frames | Where-Object { [GateWin]::IsWindowVisible($_) }).Count
    $log = Get-Content $logFile -Raw
    Check 'survives explorer restart' (-not $proc.HasExited -and $visible -gt 0 -and $visible -eq $frames.Count) "($visible/$($frames.Count) visible)"
    Check 'explorer restart logged' ($log -match 'explorer restarted')
    Test-ShowDesktop $proc 'after explorer restart'
}

Write-Host "`n--- deskframe.log (tail) ---"
Get-Content $logFile -Tail 15

if (-not $KeepRunning) { Stop-Process -Id $proc.Id -ErrorAction SilentlyContinue }
if ($script:failed -gt 0) { Write-Host "`n$($script:failed) check(s) failed" -ForegroundColor Red; exit 1 }
Write-Host "`nAll checks passed" -ForegroundColor Green
exit 0
