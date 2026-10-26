param([string]$Exe,[string]$Out)
Add-Type -AssemblyName System.Drawing
$ic=[System.Drawing.Icon]::ExtractAssociatedIcon($Exe)
$bmp=$ic.ToBitmap()
$bmp.Save($Out)
$bmp.Dispose()
$ic.Dispose()
Write-Output ("extracted icon size " + $ic.Width + "x" + $ic.Height + " -> " + $Out)
