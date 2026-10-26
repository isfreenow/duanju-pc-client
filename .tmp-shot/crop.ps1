param([string]$In,[string]$Out,[int]$X,[int]$Y,[int]$W,[int]$H,[double]$S=4)
Add-Type -AssemblyName System.Drawing
$i=[System.Drawing.Image]::FromFile($In)
$o=[int]($W*$S)
$p=[int]($H*$S)
$c=New-Object System.Drawing.Bitmap($o,$p)
$g=[System.Drawing.Graphics]::FromImage($c)
$g.InterpolationMode=[System.Drawing.Drawing2D.InterpolationMode]::NearestNeighbor
$g.DrawImage($i,(New-Object System.Drawing.Rectangle(0,0,$o,$p)),(New-Object System.Drawing.Rectangle($X,$Y,$W,$H)),[System.Drawing.GraphicsUnit]::Pixel)
$c.Save($Out)
$g.Dispose()
$c.Dispose()
$i.Dispose()
Write-Output ("cropped " + $Out)
