# Downloads the Sintel trailer (Blender Foundation, CC BY) and makes the DVD-style test files
# used during development. Needs ffmpeg on PATH, or pass the one the app downloaded:
#   -FFmpeg "$env:LOCALAPPDATA\AI Video Upscaler\tools\bin\ffmpeg.exe"
#
#   powershell -ExecutionPolicy Bypass -File scripts\make-test-videos.ps1 [-Out test-videos]
#
# Results (all 52 s; the film itself is 2.35:1, master 2048x872):
#   sintel_trailer-480p.mp4    854x480 16:9, letterboxed 2.35:1 picture (as published)
#   sintel_trailer-720p.mp4    1280x720, same picture (cross-check)
#   sintel_dvd_ntsc.mpg        NTSC DVD: MPEG-2 720x480 anamorphic 16:9 (SAR 32:27), interlaced, AC-3
#   sintel_dvd_pal_43.vob      PAL DVD 4:3 (SAR 16:15), letterboxed, sides cropped (picture 1.76:1)
#   sintel_dvd_pal_43_full.vob PAL DVD 4:3, picture over the whole frame (no bars)
#   sintel_wrong_flag_169.vob  2.35:1 letterboxed in a 4:3 PAL frame but flagged 16:9 (wrong on purpose)

param(
    [string]$Out = "test-videos",
    [string]$FFmpeg = "ffmpeg"
)
$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force $Out | Out-Null
$base = "https://download.blender.org/durian/trailer"
foreach ($f in "sintel_trailer-480p.mp4", "sintel_trailer-720p.mp4") {
    $dest = Join-Path $Out $f
    if (-not (Test-Path $dest)) {
        Write-Host "Downloading $f"
        Invoke-WebRequest "$base/$f" -OutFile $dest
    }
}
$src480 = Join-Path $Out "sintel_trailer-480p.mp4"
$src720 = Join-Path $Out "sintel_trailer-720p.mp4"
function Make($name, [string[]]$ffargs) {
    $dest = Join-Path $Out $name
    if (Test-Path $dest) { return }
    Write-Host "Making $name"
    & $FFmpeg -y -loglevel error @ffargs $dest
    if ($LASTEXITCODE -ne 0) { throw "ffmpeg failed for $name" }
}
Make "sintel_dvd_ntsc.mpg" @("-i", $src480, "-target", "ntsc-dvd", "-aspect", "16:9", "-flags", "+ilme+ildct", "-top", "1")
Make "sintel_dvd_pal_43.vob" @("-i", $src480, "-target", "pal-dvd", "-aspect", "4:3", "-vf", "crop=640:480,scale=720:576")
Make "sintel_dvd_pal_43_full.vob" @("-i", $src480, "-vf", "crop=484:364:185:58,scale=720:576", "-target", "pal-dvd", "-aspect", "4:3")
Make "sintel_wrong_flag_169.vob" @("-i", $src720, "-vf", "crop=1280:544:0:88,scale=720:326,pad=720:576:0:125:black", "-target", "pal-dvd", "-aspect", "16:9")
Write-Host "Done: $((Resolve-Path $Out).Path)"
