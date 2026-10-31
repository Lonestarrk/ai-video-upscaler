#!/bin/sh
# Same as make-test-videos.ps1, for macOS/Linux: ./scripts/make-test-videos.sh [out-dir]
set -e
OUT=${1:-test-videos}
mkdir -p "$OUT"
B=https://download.blender.org/durian/trailer
for f in sintel_trailer-480p.mp4 sintel_trailer-720p.mp4; do
  [ -f "$OUT/$f" ] || curl -sSL -o "$OUT/$f" "$B/$f"
done
S=$OUT/sintel_trailer-480p.mp4
S7=$OUT/sintel_trailer-720p.mp4
mk() { n=$1; shift; [ -f "$OUT/$n" ] || ffmpeg -y -loglevel error "$@" "$OUT/$n"; }
mk sintel_dvd_ntsc.mpg -i "$S" -target ntsc-dvd -aspect 16:9 -flags +ilme+ildct -top 1
mk sintel_dvd_pal_43.vob -i "$S" -target pal-dvd -aspect 4:3 -vf crop=640:480,scale=720:576
mk sintel_dvd_pal_43_full.vob -i "$S" -vf crop=484:364:185:58,scale=720:576 -target pal-dvd -aspect 4:3
mk sintel_wrong_flag_169.vob -i "$S7" -vf crop=1280:544:0:88,scale=720:326,pad=720:576:0:125:black -target pal-dvd -aspect 16:9
echo "Done: $OUT"
