param([string]$Out)
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type -MemberDefinition '[System.Runtime.InteropServices.DllImport("user32.dll")]public static extern void keybd_event(byte k, byte s, uint f, System.IntPtr e);' -Name KB -Namespace W32 -PassThru | Out-Null
[void][W32.KB]::keybd_event(0x12,0,0,[IntPtr]::Zero)
Start-Sleep -Milliseconds 250
[void][W32.KB]::keybd_event(0x09,0,0,[IntPtr]::Zero)
[void][W32.KB]::keybd_event(0x09,0,2,[IntPtr]::Zero)
Start-Sleep -Milliseconds 900
$b=[System.Windows.Forms.SystemInformation]::VirtualScreen
$i=New-Object System.Drawing.Bitmap($b.Width,$b.Height)
$g=[System.Drawing.Graphics]::FromImage($i)
$g.CopyFromScreen($b.X,$b.Y,0,0,$i.Size)
$i.Save($Out)
$g.Dispose(); $i.Dispose()
[void][W32.KB]::keybd_event(0x1B,0,0,[IntPtr]::Zero)
[void][W32.KB]::keybd_event(0x1B,0,2,[IntPtr]::Zero)
Start-Sleep -Milliseconds 200
[void][W32.KB]::keybd_event(0x12,0,2,[IntPtr]::Zero)
Write-Output ("saved " + $Out)