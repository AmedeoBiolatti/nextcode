# Draws the app icon ("<N>"): crates/ui/assets/next_code.ico (16-256 px; DIB
# entries below 256, PNG at 256) and next_code-64.png (title bar).
#   powershell -ExecutionPolicy Bypass -File scripts\make-icon.ps1
param([string]$out = "$PSScriptRoot\..\crates\ui\assets\next_code")
Add-Type -AssemblyName System.Drawing
# The background ramp, blue #3586f6 to violet #7c55ec, sampled in OKLab:
# 1024 sRGB colors (0-255, unrounded).
function ToLinear([double]$c) { $c /= 255; if ($c -le 0.04045) { $c / 12.92 } else { [Math]::Pow(($c + 0.055) / 1.055, 2.4) } }
function ToSrgb([double]$c) { $c = [Math]::Min(1.0, [Math]::Max(0.0, $c)); 255 * $(if ($c -le 0.0031308) { 12.92 * $c } else { 1.055 * [Math]::Pow($c, 1 / 2.4) - 0.055 }) }
function ToOklab([double[]]$rgb) {
  $r = ToLinear $rgb[0]; $g = ToLinear $rgb[1]; $b = ToLinear $rgb[2]
  $l = [Math]::Pow(0.4122214708 * $r + 0.5363325363 * $g + 0.0514459929 * $b, 1 / 3)
  $m = [Math]::Pow(0.2119034982 * $r + 0.6806995451 * $g + 0.1073969566 * $b, 1 / 3)
  $q = [Math]::Pow(0.0883024619 * $r + 0.2817188376 * $g + 0.6299787005 * $b, 1 / 3)
  @((0.2104542553 * $l + 0.7936177850 * $m - 0.0040720468 * $q),
    (1.9779984951 * $l - 2.4285922050 * $m + 0.4505937099 * $q),
    (0.0259040371 * $l + 0.7827717662 * $m - 0.8086757660 * $q))
}
function FromOklab([double[]]$lab) {
  $l = [Math]::Pow($lab[0] + 0.3963377774 * $lab[1] + 0.2158037573 * $lab[2], 3)
  $m = [Math]::Pow($lab[0] - 0.1055613458 * $lab[1] - 0.0638541728 * $lab[2], 3)
  $q = [Math]::Pow($lab[0] - 0.0894841775 * $lab[1] - 1.2914855480 * $lab[2], 3)
  @((ToSrgb (4.0767416621 * $l - 3.3077115913 * $m + 0.2309699292 * $q)),
    (ToSrgb (-1.2684380046 * $l + 2.6097574011 * $m - 0.3413193965 * $q)),
    (ToSrgb (-0.0041960863 * $l - 0.7034186147 * $m + 1.7076147010 * $q)))
}
$from = ToOklab @(0x35, 0x86, 0xf6); $to = ToOklab @(0x7c, 0x55, 0xec)
$ramp = foreach ($j in 0..1023) {
  $t = $j / 1023
  ,(FromOklab @(0..2 | ForEach-Object { $from[$_] + ($to[$_] - $from[$_]) * $t }))
}
$sizes = 16, 20, 24, 32, 40, 48, 64, 128, 256
$pngs = @()
foreach ($s in $sizes) {
  $bmp = New-Object System.Drawing.Bitmap $s, $s, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.SmoothingMode = 'AntiAlias'; $g.PixelOffsetMode = 'HighQuality'; $g.Clear([System.Drawing.Color]::Transparent)
  $m = [Math]::Max(0.5, $s * 0.03)
  $w = $s - 2 * $m
  $r = $w * 0.24
  $path = New-Object System.Drawing.Drawing2D.GraphicsPath
  $path.AddArc($m, $m, 2*$r, 2*$r, 180, 90)
  $path.AddArc($m + $w - 2*$r, $m, 2*$r, 2*$r, 270, 90)
  $path.AddArc($m + $w - 2*$r, $m + $w - 2*$r, 2*$r, 2*$r, 0, 90)
  $path.AddArc($m, $m + $w - 2*$r, 2*$r, 2*$r, 90, 90)
  $path.CloseFigure()
  # Background: one blue-to-violet gradient along the diagonal, computed
  # per pixel (GDI+ gradients are banded 8-bit steps): interpolated in
  # OKLab so it changes evenly with no dull middle, and dithered with
  # triangular noise below one level so no step can form. The rounded
  # shape's antialiased coverage becomes the alpha.
  $mask = New-Object System.Drawing.Bitmap $s, $s, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $mg = [System.Drawing.Graphics]::FromImage($mask)
  $mg.SmoothingMode = 'AntiAlias'; $mg.PixelOffsetMode = 'HighQuality'; $mg.Clear([System.Drawing.Color]::Transparent)
  $mg.FillPath([System.Drawing.Brushes]::White, $path); $mg.Dispose()
  $box = New-Object System.Drawing.Rectangle 0, 0, $s, $s
  $md = $mask.LockBits($box, 'ReadOnly', $mask.PixelFormat)
  $px = New-Object byte[] ($md.Stride * $s)
  [System.Runtime.InteropServices.Marshal]::Copy($md.Scan0, $px, 0, $px.Length)
  $mask.UnlockBits($md); $mask.Dispose()
  for ($y = 0; $y -lt $s; $y++) {
    for ($x = 0; $x -lt $s; $x++) {
      $i = $y * $md.Stride + 4 * $x
      $t = [Math]::Min(1.0, [Math]::Max(0.0, ($x + $y + 1 - 2 * $m) / (2 * $w)))
      $c = $ramp[[int][Math]::Round($t * ($ramp.Count - 1))]
      $n1 = [Math]::Sin($x * 12.9898 + $y * 78.233 + $s) * 43758.5453; $n1 -= [Math]::Floor($n1)
      $n2 = [Math]::Sin($x * 39.3468 + $y * 11.135 + $s) * 24634.6345; $n2 -= [Math]::Floor($n2)
      $noise = $n1 + $n2 - 1.0
      for ($k = 0; $k -lt 3; $k++) {
        $px[$i + $k] = [byte][Math]::Min(255.0, [Math]::Max(0.0, [Math]::Round($c[2 - $k] + $noise)))
      }
    }
  }
  $bd = $bmp.LockBits($box, 'WriteOnly', $bmp.PixelFormat)
  [System.Runtime.InteropServices.Marshal]::Copy($px, 0, $bd.Scan0, $px.Length)
  $bmp.UnlockBits($bd)
  # "<N>" with a slanted N, drawn as one stroke: the free end of "<"'s
  # upper arm, its tip, its lower arm into the N's bottom-left, up the N,
  # down its diagonal, up again to the top-right, which is ">"'s upper
  # arm, then ">"'s tip and its free lower arm. Both arms of each chevron
  # run at 45 degrees, the N's two uprights are parallel, and the figure
  # is point-symmetric about the center. Small sizes snap the points to
  # the pixel grid (pixel centers for an odd stroke) so lines stay crisp.
  $stroke = [Math]::Max(1.5, $s * 0.08)
  $snap = $s -le 48
  if ($snap) { $stroke = [Math]::Max(1, [Math]::Round($stroke)) }
  $pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::White), $stroke
  $pen.StartCap = 'Round'; $pen.EndCap = 'Round'; $pen.LineJoin = 'Round'
  $half = if ($snap -and ($stroke % 2) -eq 1) { 0.5 } else { 0.0 }
  function P([double]$x, [double]$y) {
    $px = $x * $s; $py = $y * $s
    if ($snap) { $px = [Math]::Round($px - $half) + $half; $py = [Math]::Round($py - $half) + $half }
    New-Object System.Drawing.PointF $px, $py
  }
  $tl = P 0.44 0.28; $bl = P 0.37 0.72; $tr = P 0.63 0.28; $br = P 0.56 0.72
  $lt = P 0.15 0.50; $rt = P 0.85 0.50
  $upperEnd = P 0.27 0.38; $lowerEnd = P 0.73 0.62
  $stroke_points = [System.Drawing.PointF[]]@($upperEnd, $lt, $bl, $tl, $br, $tr, $rt, $lowerEnd)
  $g.DrawLines($pen, $stroke_points)
  $ms = New-Object System.IO.MemoryStream
  if ($s -ge 256) {
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
  } else {
    # Classic 32-bit DIB entry: header, bottom-up BGRA rows, AND mask.
    $dw = New-Object System.IO.BinaryWriter $ms
    $dw.Write([UInt32]40); $dw.Write([Int32]$s); $dw.Write([Int32](2 * $s))
    $dw.Write([UInt16]1); $dw.Write([UInt16]32); $dw.Write([UInt32]0)
    $maskStride = [int]([Math]::Ceiling($s / 32.0) * 4)
    $dw.Write([UInt32]($s * $s * 4 + $maskStride * $s))
    $dw.Write([Int32]0); $dw.Write([Int32]0); $dw.Write([UInt32]0); $dw.Write([UInt32]0)
    for ($y = $s - 1; $y -ge 0; $y--) {
      for ($x = 0; $x -lt $s; $x++) {
        $c = $bmp.GetPixel($x, $y)
        $dw.Write([byte]$c.B); $dw.Write([byte]$c.G); $dw.Write([byte]$c.R); $dw.Write([byte]$c.A)
      }
    }
    $dw.Write((New-Object byte[] ($maskStride * $s)))
    $dw.Flush()
  }
  if ($s -eq 64) { $bmp.Save("$out-64.png") }
  $pngs += ,@($s, $ms.ToArray())
  $g.Dispose(); $bmp.Dispose()
}
# ICO directory, then the entries.
$fs = [System.IO.File]::Create("$out.ico")
$bw = New-Object System.IO.BinaryWriter $fs
$bw.Write([UInt16]0); $bw.Write([UInt16]1); $bw.Write([UInt16]$pngs.Count)
$offset = 6 + 16 * $pngs.Count
foreach ($p in $pngs) {
  $s = $p[0]; $data = $p[1]
  $dim = if ($s -ge 256) { 0 } else { $s }
  $bw.Write([byte]$dim); $bw.Write([byte]$dim); $bw.Write([byte]0); $bw.Write([byte]0)
  $bw.Write([UInt16]1); $bw.Write([UInt16]32)
  $bw.Write([UInt32]$data.Length); $bw.Write([UInt32]$offset)
  $offset += $data.Length
}
foreach ($p in $pngs) { $bw.Write([byte[]]$p[1]) }
$bw.Close()
"wrote $out.ico and $out-64.png"
