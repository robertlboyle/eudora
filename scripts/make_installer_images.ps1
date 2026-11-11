# Draws the installer's pictures from Eudora's icon (packaging\images\): the tall picture on the
# Welcome and Finished pages (blue panel, the icon on a white tile, "Eudora 27") and the small one
# at the top right of the other pages, at every size Inno Setup lists for 100-250 % scaling (Setup
# picks the one for the screen). As PlanetPDF's.
#
#   .\scripts\make_installer_images.ps1
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$root = Split-Path $PSScriptRoot
$icon = [System.Drawing.Image]::FromFile((Join-Path $root 'crates\eudora-app\icons\icon.png'))
$out = Join-Path $root 'packaging\images'
New-Item -ItemType Directory -Force $out | Out-Null

function New-Canvas([int]$w, [int]$h) {
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.InterpolationMode = 'HighQualityBicubic'
    $g.TextRenderingHint = 'AntiAliasGridFit'
    return $bmp, $g
}

# the tall picture: 164x314 at 100 % up to 410x797 at 250 %
foreach ($size in '164x314', '192x386', '246x459', '273x556', '328x604', '355x700', '410x797') {
    $w, $h = $size.Split('x') | ForEach-Object { [int]$_ }
    $bmp, $g = New-Canvas $w $h
    $top = [System.Drawing.Color]::FromArgb(255, 18, 64, 140)
    $bottom = [System.Drawing.Color]::FromArgb(255, 8, 34, 82)
    $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush (New-Object System.Drawing.Rectangle 0, 0, $w, $h), $top, $bottom, 90
    $g.FillRectangle($brush, 0, 0, $w, $h)
    # the icon on a white rounded tile
    $tile = [int]($w * 0.62)
    $x = [int](($w - $tile) / 2); $y = [int]($h * 0.22)
    $r = [int]($tile * 0.16)
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddArc($x, $y, $r * 2, $r * 2, 180, 90)
    $path.AddArc($x + $tile - $r * 2, $y, $r * 2, $r * 2, 270, 90)
    $path.AddArc($x + $tile - $r * 2, $y + $tile - $r * 2, $r * 2, $r * 2, 0, 90)
    $path.AddArc($x, $y + $tile - $r * 2, $r * 2, $r * 2, 90, 90)
    $path.CloseFigure()
    $g.FillPath([System.Drawing.Brushes]::White, $path)
    $pad = [int]($tile * 0.1)
    $g.DrawImage($icon, $x + $pad, $y + $pad, $tile - 2 * $pad, $tile - 2 * $pad)
    # the name under it
    $font = New-Object System.Drawing.Font 'Segoe UI Semibold', ([float]($w * 0.135)), ([System.Drawing.FontStyle]::Regular), ([System.Drawing.GraphicsUnit]::Pixel)
    $format = New-Object System.Drawing.StringFormat
    $format.Alignment = 'Center'
    $g.DrawString('Eudora', $font, [System.Drawing.Brushes]::White, (New-Object System.Drawing.RectangleF 0, ($y + $tile + $h * 0.05), $w, ($h * 0.2)), $format)
    $small = New-Object System.Drawing.Font 'Segoe UI', ([float]($w * 0.07)), ([System.Drawing.FontStyle]::Regular), ([System.Drawing.GraphicsUnit]::Pixel)
    $soft = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(210, 255, 255, 255))
    $g.DrawString('Mail for decades of messages', $small, $soft, (New-Object System.Drawing.RectangleF 0, ($y + $tile + $h * 0.15), $w, ($h * 0.1)), $format)
    $g.Dispose()
    $bmp.Save((Join-Path $out "wizard-$size.png"), [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
}

# the small picture (transparent): 55x55 at 100 % up to 138x140 at 250 %
foreach ($size in '55x55', '64x68', '69x69', '83x80', '92x97', '110x106', '119x123', '138x140') {
    $w, $h = $size.Split('x') | ForEach-Object { [int]$_ }
    $bmp, $g = New-Canvas $w $h
    $side = [Math]::Min($w, $h)
    $g.DrawImage($icon, [int](($w - $side) / 2), [int](($h - $side) / 2), $side, $side)
    $g.Dispose()
    $bmp.Save((Join-Path $out "small-$size.png"), [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
}
$icon.Dispose()
Write-Output "Installer pictures in $out"
