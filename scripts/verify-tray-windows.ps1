param(
    [Parameter(Mandatory)][string]$Executable,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [switch]$TestStartup,
    [ValidateSet('tray','taskbar')][string]$Mode = 'tray',
    [switch]$CheckRoundedCorners
)
$ErrorActionPreference = 'Stop'
$Executable = (Resolve-Path -LiteralPath $Executable).Path
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null

# Read-only native checks. No simulated mouse/keyboard input or client changes.
if (-not ('DashboardTrayProbe' -as [type])) { Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class DashboardTrayProbe {
    [StructLayout(LayoutKind.Sequential)] struct Identifier {
        public uint size; public IntPtr window; public uint id; public Guid guid;
    }
    [StructLayout(LayoutKind.Sequential)] struct Rect {
        public int left, top, right, bottom;
    }
    [DllImport("user32.dll", CharSet=CharSet.Unicode)]
    static extern IntPtr FindWindowEx(IntPtr parent, IntPtr after, string name, string title);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    delegate bool WindowEnumerator(IntPtr window, IntPtr parameter);
    [DllImport("user32.dll")] static extern bool EnumWindows(WindowEnumerator callback, IntPtr parameter);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetClassName(IntPtr window, System.Text.StringBuilder name, int count);
    [DllImport("dwmapi.dll")] static extern int DwmGetWindowAttribute(IntPtr window, int attribute, out int value, int size);
    [DllImport("shell32.dll")] static extern int Shell_NotifyIconGetRect(ref Identifier id, out Rect rect);
    [DllImport("shell32.dll", CharSet=CharSet.Unicode)]
    public static extern uint ExtractIconEx(string file, int index, IntPtr large, IntPtr small, uint count);
    public static bool Visible(uint pid) {
        bool visible = false;
        EnumWindows((window, parameter) => { uint owner; GetWindowThreadProcessId(window, out owner); if (owner == pid) { var name = new System.Text.StringBuilder(256); GetClassName(window, name, 256); if (name.ToString() == "Window Class" && IsWindowVisible(window)) visible = true; } return true; }, IntPtr.Zero);
        return visible;
    }
    public static int CornerPreference(uint pid) {
        int preference = -1;
        EnumWindows((window, parameter) => { uint owner; GetWindowThreadProcessId(window, out owner); if (owner == pid) { var name = new System.Text.StringBuilder(256); GetClassName(window, name, 256); if (name.ToString() == "Window Class") { int value; if (DwmGetWindowAttribute(window, 33, out value, 4) == 0) preference = value; } } return true; }, IntPtr.Zero);
        return preference;
    }
    public static bool Registered(uint pid) {
        IntPtr after = IntPtr.Zero;
        while (true) {
            after = FindWindowEx(new IntPtr(-3), after, "SlintSystemTrayWindow", null);
            if (after == IntPtr.Zero) return false;
            uint owner; GetWindowThreadProcessId(after, out owner);
            if (owner != pid) continue;
            var id = new Identifier { size=(uint)Marshal.SizeOf(typeof(Identifier)), window=after, id=1 };
            Rect rect;
            if (Shell_NotifyIconGetRect(ref id, out rect) == 0) return true;
        }
    }
}
'@
}
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$valueName = 'AI Tool Dashboard'
function Read-StartupValue {
    $item = Get-ItemProperty -LiteralPath $runKey -ErrorAction SilentlyContinue
    if ($item) {
        $property = $item.PSObject.Properties[$valueName]
        if ($property) { $property.Value }
    }
}
$before = Read-StartupValue
if ($TestStartup -and $null -ne $before) { throw 'Startup verification requires an empty application startup slot; existing value preserved.' }
$state = Join-Path $OutputDirectory 'window.json'
$temporaryCommand = '"' + $Executable + '" --background --state "' + $state + '"'
$env:AI_DASHBOARD_VERIFY_DIR = $OutputDirectory
$env:AI_DASHBOARD_VERIFY_MODE = if ($TestStartup) { 'tray-startup' } else { $Mode }
$env:AI_DASHBOARD_RENDERER = 'software'
$process = $null
$registered = $false
$taskbarChecks = @()
$lastStage = ''
if ($Mode -eq 'taskbar') {
    Add-Type -AssemblyName UIAutomationClient
    Add-Type -AssemblyName UIAutomationTypes
    if ($CheckRoundedCorners) { Add-Type -AssemblyName System.Drawing }
    if (![System.Windows.Automation.AutomationElement]::RootElement) { throw 'Desktop UI Automation unavailable; run with Windows PowerShell 5.1.' }
    foreach ($name in @('taskbar-stage.json','taskbar-ack.txt','error.txt')) {
        $old = Join-Path $OutputDirectory $name
        if (Test-Path -LiteralPath $old) { Remove-Item -LiteralPath $old }
    }
}
try {
    $process = Start-Process -FilePath $Executable -ArgumentList @('--background', '--state', ('"' + $state + '"')) -WindowStyle Hidden -PassThru
    $watch = [Diagnostics.Stopwatch]::StartNew()
    while (!$process.HasExited -and $watch.Elapsed.TotalSeconds -lt 40) {
        if ([DashboardTrayProbe]::Registered($process.Id)) { $registered = $true }
        if ($Mode -eq 'taskbar' -and (Test-Path -LiteralPath (Join-Path $OutputDirectory 'taskbar-stage.json'))) {
            $stage = $null
            try { $stage = Get-Content -LiteralPath (Join-Path $OutputDirectory 'taskbar-stage.json') -Raw | ConvertFrom-Json } catch { }
            if ($stage -and $stage.pid -eq $process.Id -and $stage.name -ne $lastStage) {
                $title = 'AI Tool Dashboard Verification ' + $process.Id
                $appId = 'AI.Tool.Dashboard.Verification.' + $process.Id
                $bars = [System.Windows.Automation.AutomationElement]::RootElement.FindAll(
                    [System.Windows.Automation.TreeScope]::Children,
                    [System.Windows.Automation.OrCondition]::new(
                        [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::ClassNameProperty, 'Shell_TrayWnd'),
                        [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::ClassNameProperty, 'Shell_SecondaryTrayWnd')))
                $taskbarButtonCount = 0
                foreach ($bar in $bars) {
                    $buttons = $bar.FindAll([System.Windows.Automation.TreeScope]::Descendants,
                        [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::ControlTypeProperty, [System.Windows.Automation.ControlType]::Button))
                    foreach ($button in $buttons) {
                        if ($button.Current.Name.Contains($title) -or $button.Current.AutomationId.Contains($appId)) { $taskbarButtonCount++ }
                    }
                }
                $expectedButton = $stage.visible -and !$stage.compact
                $visible = [DashboardTrayProbe]::Visible($process.Id)
                $trayPresent = [DashboardTrayProbe]::Registered($process.Id)
                $cornerPreference = [DashboardTrayProbe]::CornerPreference($process.Id)
                [pscustomobject]@{stage=$stage.name;compact=$stage.compact;windowVisible=$visible;taskbarButtons=$taskbarButtonCount;trayRegistered=$trayPresent} |
                    ConvertTo-Json | Set-Content -LiteralPath (Join-Path $OutputDirectory 'taskbar-observation.json') -Encoding utf8
                [pscustomobject]@{stage=$stage.name;windowVisible=$visible;taskbarButtons=$taskbarButtonCount;trayRegistered=$trayPresent} |
                    ConvertTo-Json -Compress | Add-Content -LiteralPath (Join-Path $OutputDirectory 'taskbar-observation-history.jsonl') -Encoding utf8
                if ($bars.Count -gt 0 -and (($taskbarButtonCount -gt 0) -eq $expectedButton) -and ($visible -eq $stage.visible) -and $trayPresent -and (!$CheckRoundedCorners -or $cornerPreference -eq 2)) {
                    if ($CheckRoundedCorners -and $stage.name -in @('compact','full')) {
                        try {
                            $main = [System.Windows.Automation.AutomationElement]::RootElement.FindFirst([System.Windows.Automation.TreeScope]::Children,
                                [System.Windows.Automation.AndCondition]::new(
                                    [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::ProcessIdProperty, [int]$process.Id),
                                    [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::ClassNameProperty, 'Window Class')))
                            $bounds = $main.Current.BoundingRectangle
                            $bitmap = [System.Drawing.Bitmap]::new([int]$bounds.Width,[int]$bounds.Height)
                            $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
                            try {
                                $graphics.CopyFromScreen([int]$bounds.X,[int]$bounds.Y,0,0,$bitmap.Size)
                                $bitmap.Save((Join-Path $OutputDirectory ('native-' + $stage.name + '.png')), [System.Drawing.Imaging.ImageFormat]::Png)
                            } finally { $graphics.Dispose(); $bitmap.Dispose() }
                        } catch { 'Native screen capture unavailable' | Set-Content -LiteralPath (Join-Path $OutputDirectory 'screen-capture-unavailable.txt') }
                    }
                    $taskbarChecks += [pscustomobject]@{stage=$stage.name;compact=$stage.compact;windowVisible=$visible;taskbarButtons=$taskbarButtonCount;trayRegistered=$trayPresent;cornerPreference=$cornerPreference}
                    $taskbarChecks | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $OutputDirectory 'taskbar-native.json') -Encoding utf8
                    $lastStage = $stage.name
                    [IO.File]::WriteAllText((Join-Path $OutputDirectory 'taskbar-ack.txt'), $stage.name)
                }
            }
        }
        [void]$process.WaitForExit(100)
    }
    if (!$process.HasExited) { throw 'Tray verification timed out' }
    if ($process.ExitCode -ne 0) { throw 'Tray verification exited with an error' }
    if (Test-Path -LiteralPath (Join-Path $OutputDirectory 'error.txt')) {
        throw (Get-Content -LiteralPath (Join-Path $OutputDirectory 'error.txt') -Raw)
    }
    if (!$registered) { throw 'Windows Shell did not confirm the tray icon' }
    $report = Get-Content -LiteralPath (Join-Path $OutputDirectory 'report.json') -Raw | ConvertFrom-Json
    $iconCount = [DashboardTrayProbe]::ExtractIconEx($Executable, -1, [IntPtr]::Zero, [IntPtr]::Zero, 0)
    if ($iconCount -lt 1) { throw 'No PE icon resource found' }
    $removed = -not [DashboardTrayProbe]::Registered($process.Id)
    if (!$removed) { throw 'Tray icon was not removed on exit' }
    if ($Mode -eq 'taskbar') {
        $taskbarChecks | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $OutputDirectory 'taskbar-native.json') -Encoding utf8
        if ($taskbarChecks.Count -ne 8) { throw 'Not all taskbar stages were verified' }
    }
    $native = [pscustomobject]@{shellRegistered=$registered; shellRemovedAfterExit=$removed; peIconGroups=$iconCount; exitCode=$process.ExitCode}
    $native | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $OutputDirectory 'native.json') -Encoding utf8
    $native
    $report | ConvertTo-Json -Depth 8
} finally {
    if ($process -and !$process.HasExited) { Stop-Process -Id $process.Id; $process.WaitForExit() }
    if ($TestStartup) {
        $after = Read-StartupValue
        if ($after -eq $temporaryCommand) { Remove-ItemProperty -LiteralPath $runKey -Name $valueName }
        elseif ($null -ne $after) { throw 'Startup slot changed unexpectedly; it was preserved.' }
    }
}
