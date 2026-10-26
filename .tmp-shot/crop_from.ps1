param([string]$SrcPath,[string]$Out,[int]$X,[int]$Y,[int]$W,[int]$H,[int]$S)
Add-Type -AssemblyName System.Drawing
$i=[System.Drawing.Image]::FromFile($SrcPath)
$r=New-Object System.Drawing.Rectangle($X,$Y,$W,$H)
$c=New-Object System.Drawing.Bitmap($W*$S,$H*$S)
$g=[System.Drawing.Graphics]::FromImage($c)
$g.InterpolationMode=[System.Drawing.Drawing2D.InterpolationMode]::NearestNeighbor
$g.DrawImage($i,(New-Object System.Drawing.Rectangle(0,0,$W*$S,$H*$S)),$r,[System.Drawing.GraphicsUnit]::Pixel)
$c.Save($Out)
$g.Dispose();$c.Dispose();$i.Dispose()
Write-Output ("cropped " + $Out)
