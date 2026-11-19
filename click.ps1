# Dev helper: bring WizEverything to the front, right-click at (x, y) relative
# to the window, wait and save a screenshot. Usage: .\click.ps1 220 160
param([int]$X = 220, [int]$Y = 160, [string]$Out = "$PSScriptRoot\shot.png", [switch]$Left)
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices;
public class M {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(int f, int dx, int dy, int d, int e);
  [DllImport("user32.dll")] public static extern void keybd_event(byte k, byte s, int f, int e);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  public struct RECT { public int L, T, R, B; }
}
"@
[M]::SetProcessDPIAware() | Out-Null
$h = (Get-Process wizeverything | Where-Object MainWindowHandle -ne 0 | Select-Object -First 1).MainWindowHandle
[M]::ShowWindow($h, 3) | Out-Null
[M]::keybd_event(0x12, 0, 0, 0); [M]::keybd_event(0x12, 0, 2, 0)   # ALT tap lets SetForegroundWindow succeed
[M]::SetForegroundWindow($h) | Out-Null
Start-Sleep -Milliseconds 600
$r = New-Object M+RECT; [M]::GetWindowRect($h, [ref]$r) | Out-Null
[M]::SetCursorPos($r.L + $X, $r.T + $Y) | Out-Null
Start-Sleep -Milliseconds 200
if ($Left) { [M]::mouse_event(0x0002, 0, 0, 0, 0); Start-Sleep -Milliseconds 60; [M]::mouse_event(0x0004, 0, 0, 0, 0) } else { [M]::mouse_event(0x0008, 0, 0, 0, 0); Start-Sleep -Milliseconds 60; [M]::mouse_event(0x0010, 0, 0, 0, 0) }
Start-Sleep -Milliseconds 700
$bmp = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.L, $r.T, 0, 0, $bmp.Size)
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
"saved $Out"
