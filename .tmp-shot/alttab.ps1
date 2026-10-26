Add-Type -TypeDefinition "using System;using System.Runtime.InteropServices;public class KB{[DllImport(`"user32.dll`")]public static extern void keybd_event(byte k,byte s,uint f,System.IntPtr e);}"
$d="D:\workspace\Codespace\duanju-pc-client\.tmp-shot"
$alt=18
$tab=9
$esc=27
[KB]::keybd_event($alt,0,0,[IntPtr]::Zero)
[KB]::keybd_event($tab,0,0,[IntPtr]::Zero)
[KB]::keybd_event($tab,0,2,[IntPtr]::Zero)
Start-Sleep -Milliseconds 900
& ($d + "\cap2.ps1") -Out ($d + "\I_alttab_full.png")
[KB]::keybd_event($esc,0,0,[IntPtr]::Zero)
[KB]::keybd_event($esc,0,2,[IntPtr]::Zero)
[KB]::keybd_event($alt,0,2,[IntPtr]::Zero)
Start-Sleep -Milliseconds 600
& ($d + "\cap2.ps1") -Out ($d + "\J_after_alttab.png")
Write-Output "alttab sequence done"
