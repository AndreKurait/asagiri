# Writes the version onto the MSI's dialog bitmap.
#
# The committed art (wix-dialog.bmp 493x312, wix-banner.bmp 493x58, 24-bit BMP,
# the only format MSI takes) is drawn by brand/render.py from brand/logo: a
# 164 px night panel with the stacked lockup beside a white field the WixUI
# text sits on, and a light strip with the horizontal lockup at its right.
# render.py leaves the line under the lockup empty.
#
#   make-bitmaps.ps1 -Stamp -Version 0.1.0 -Out <dir>
#     Copies the committed art to <dir> and writes the version under the
#     lockup with System.Drawing: no browser, so a CI runner does it from
#     the checkout. `cargo xtask msi` runs this.
param(
    [switch]$Stamp,
    [string]$Version = '0.1.0',
    [string]$Out = $PSScriptRoot
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

# Where the version goes, in the 493x312 dialog: centred in the panel, under
# the lockup (render.py reserves this line, so the two agree).
$PanelWidth = 164
$VersionTop = 225
$VersionHeight = 11
$VersionColor = [System.Drawing.Color]::FromArgb(0xA9, 0xB4, 0xD6)

function Save-Bmp([System.Drawing.Bitmap]$Bitmap, [string]$Path) {
    # Through a 24-bit copy: Bitmap.Save writes the pixel format it holds.
    $flat = New-Object System.Drawing.Bitmap $Bitmap.Width, $Bitmap.Height, ([System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
    $g = [System.Drawing.Graphics]::FromImage($flat)
    $g.DrawImageUnscaled($Bitmap, 0, 0); $g.Dispose()
    $flat.Save($Path, [System.Drawing.Imaging.ImageFormat]::Bmp); $flat.Dispose()
}

if (-not $Stamp) { throw 'make-bitmaps.ps1 only stamps a version (-Stamp); brand/render.py draws the art' }
if ($Stamp) {
    New-Item -ItemType Directory -Force $Out | Out-Null
    Copy-Item (Join-Path $PSScriptRoot 'wix-banner.bmp') (Join-Path $Out 'wix-banner.bmp') -Force
    $src = [System.Drawing.Bitmap]::FromFile((Join-Path $PSScriptRoot 'wix-dialog.bmp'))
    $bmp = New-Object System.Drawing.Bitmap $src; $src.Dispose()
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
    $installed = (New-Object System.Drawing.Text.InstalledFontCollection).Families.Name
    $face = @('JetBrains Mono', 'Cascadia Mono', 'Consolas', 'Lucida Console') | Where-Object { $installed -contains $_ } | Select-Object -First 1
    $font = New-Object System.Drawing.Font $face, 7.5, ([System.Drawing.FontStyle]::Regular), ([System.Drawing.GraphicsUnit]::Point)
    $fmt = New-Object System.Drawing.StringFormat
    $fmt.Alignment = 'Center'; $fmt.LineAlignment = 'Center'; $fmt.FormatFlags = 'NoWrap'
    $brush = New-Object System.Drawing.SolidBrush $VersionColor
    $rect = [System.Drawing.RectangleF]::new(0, $VersionTop - 2, $PanelWidth, $VersionHeight + 4)
    $g.DrawString("$Version $([char]0xB7) x64", $font, $brush, $rect, $fmt)
    $g.Dispose(); $font.Dispose(); $brush.Dispose()
    Save-Bmp $bmp (Join-Path $Out 'wix-dialog.bmp'); $bmp.Dispose()
    Write-Host "stamped $Version ($face) onto $(Join-Path $Out 'wix-dialog.bmp'); copied wix-banner.bmp"
}
