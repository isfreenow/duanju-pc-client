param([string]$Out,[int]$X=-1,[int]$Y=0,[int]$W=0,[int]$H=0,[double]$S=2)
Add-Type -TypeDefinition "using System;using System.Runtime.InteropServices;public class DPIK{[DllImport(`"user32.dll`")]public static extern bool SetProcessDPIAware();}"
[void][DPIK]::SetProcessDPIAware()
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
$b=[System.Windows.Forms.SystemInformation]::VirtualScreen
$i=New-Object System.Drawing.Bitmap($b.Width,$b.Height)
$g=[System.Drawing.Graphics]::FromImage($i)
$g.CopyFromScreen($b.X,$b.Y,0,0,$i.Size)
if($X -ge 0){
 $r=New-Object System.Drawing.Rectangle(($X-$b.X),$Y,$W,$H)
$o=[int]($W*$S)
$p2=[int]($H*$S)
$c=New-Object System.Drawing.Bitmap($o,$p2)
$g2=[System.Drawing.Graphics]::FromImage($c)
$g2.InterpolationMode=[System.Drawing.Drawing2D.InterpolationMode]::NearestNeighbor
$g2.DrawImage($i,(New-Object System.Drawing.Rectangle(0,0,$o,$p2)),$r,[System.Drawing.GraphicsUnit]::Pixel)
$c.Save($Out)
$c.Dispose()
$g2.Dispose()
}
else{
 $i.Save($Out)
}
$g.Dispose()
$i.Dispose()
Write-Output ("saved " + $Out + " vs=" + $b.Width + "x" + $b.Height + " ox=" + $b.X + " oy=" + $b.Y)
