# Draws the app icon ("<N>"): crates/ui/assets/next_code.ico (16-256 px; DIB
# entries below 256, PNG at 256) and next_code-64.png (title bar).
#   powershell -ExecutionPolicy Bypass -File scripts\make-icon.ps1
param([string]$out = "$PSScriptRoot\..\crates\ui\assets\next_code")
Add-Type -AssemblyName System.Drawing
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
  $rect = New-Object System.Drawing.RectangleF $m, $m, $w, $w
  $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush $rect, ([System.Drawing.Color]::FromArgb(255, 0x2f, 0x7b, 0xf5)), ([System.Drawing.Color]::FromArgb(255, 0x8b, 0x4d, 0xf0)), 45.0
  $g.FillPath($brush, $path)
  # Soft top highlight.
  if ($s -ge 32) {
    $hl = New-Object System.Drawing.RectangleF $m, $m, $w, ($w * 0.5)
    $hb = New-Object System.Drawing.Drawing2D.LinearGradientBrush $hl, ([System.Drawing.Color]::FromArgb(46, 255, 255, 255)), ([System.Drawing.Color]::FromArgb(0, 255, 255, 255)), 90.0
    $g.SetClip($path); $g.FillRectangle($hb, $hl); $g.ResetClip()
  }
  # "<N>" with a slanted N: one zigzag runs from the end of "<"'s lower arm
  # through its tip, up into the N's top-left, down the N's diagonal, and
  # on through ">" (merged at the N's bottom-right); the N's two stems are
  # separate strokes.
  $stroke = [Math]::Max(1.5, $s * 0.08)
  $pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::White), $stroke
  $pen.StartCap = 'Round'; $pen.EndCap = 'Round'; $pen.LineJoin = 'Round'
  function P([double]$x, [double]$y) { New-Object System.Drawing.PointF ($x * $s), ($y * $s) }
  $tl = P 0.44 0.28; $bl = P 0.37 0.72; $tr = P 0.63 0.28; $br = P 0.56 0.72
  $lt = P 0.14 0.50; $rt = P 0.86 0.50
  $lowerEnd = P 0.245 0.585; $upperEnd = P 0.755 0.415
  $zigzag = [System.Drawing.PointF[]]@($lowerEnd, $lt, $tl, $br, $rt, $upperEnd)
  $g.DrawLines($pen, $zigzag)
  $g.DrawLine($pen, $bl, $tl)
  $g.DrawLine($pen, $br, $tr)
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
