#!/usr/bin/env bash
# Prepare the project website media in site/media/ from the raw captures in assets/.
#
#   scripts/site-media.sh
#
# Screen captures are cropped below the browser's tab and bookmarks bars (rows 0-120 of a
# 1920x1080 capture) so only the n2link editor is published. The raw files stay out of git.
# Requires ffmpeg with libx264, and python3 with Pillow.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

RAW_VIDEO="assets/videos/Screencast from 2026-10-07 22-42-43.webm"
RAW_SHOTS="assets/imagecaptures"
OUT="site/media"
CROP_TOP=121          # first row of the n2link UI in a 1920x1080 browser capture
VIDEO_START=31        # skips the terminal, login and Node-RED welcome dialog
VIDEO_WIDTH=1440      # the capture is variable-frame-rate; the encode pins it to 24 fps

log() { printf '[site-media] %s\n' "$*"; }
need() { command -v "$1" >/dev/null || { log "missing: $1"; exit 1; }; }

check_inputs() {
  need ffmpeg; need ffprobe; need python3
  local encoders; encoders="$(ffmpeg -hide_banner -encoders 2>/dev/null)"
  [[ "$encoders" == *libx264* ]] || { log "ffmpeg lacks libx264"; exit 1; }
  python3 -c 'import PIL' 2>/dev/null || { log "python3 lacks Pillow"; exit 1; }
  [ -f "$RAW_VIDEO" ] || { log "missing $RAW_VIDEO"; exit 1; }
  local size; size="$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0 "$RAW_VIDEO")"
  [ "$size" = "1920,1080" ] || { log "expected a 1920x1080 video, got $size"; exit 1; }
}

# name:source pairs; the source is a file in $RAW_SHOTS.
SHOTS=(
  "copilot-cpu-flow:Screenshot from 2026-10-07 22-49-25.png"
  "debug-sidebar:Screenshot from 2026-10-07 22-49-35.png"
  "egress-policy:Screenshot from 2026-10-07 22-51-30.png"
)

crop_screenshots() {
  local pair name src
  for pair in "${SHOTS[@]}"; do
    name="${pair%%:*}"; src="$RAW_SHOTS/${pair#*:}"
    [ -f "$src" ] || { log "missing $src"; exit 1; }
    python3 -I - "$src" "$OUT/$name" "$CROP_TOP" <<'PY'
import sys
from PIL import Image
src, dst, top = sys.argv[1], sys.argv[2], int(sys.argv[3])
im = Image.open(src).convert("RGB")
if im.size != (1920, 1080):
    sys.exit(f"{src}: expected 1920x1080, got {im.size}")
im = im.crop((0, top, im.width, im.height))
im.save(dst + ".webp", "WEBP", quality=86, method=6)
im.resize((960, round(im.height * 960 / im.width)), Image.LANCZOS).save(dst + "-960.webp", "WEBP", quality=84, method=6)
PY
    log "screenshot $name.webp"
  done
}

encode_video() {
  local h=$(( (1080 - CROP_TOP) / 2 * 2 ))
  ffmpeg -v error -y -ss "$VIDEO_START" -i "$RAW_VIDEO" -an \
    -vf "fps=24,crop=1920:$h:0:$CROP_TOP,scale=$VIDEO_WIDTH:-2:flags=lanczos" \
    -c:v libx264 -preset slow -crf 28 -tune stillimage -pix_fmt yuv420p -movflags +faststart \
    "$OUT/copilot-demo.mp4"
  log "video copilot-demo.mp4 ($(du -h "$OUT/copilot-demo.mp4" | cut -f1))"
  # Poster: the finished flow from the first screenshot.
  python3 -I -c 'import sys; from PIL import Image; Image.open(sys.argv[1]).convert("RGB").resize((1440, 719)).save(sys.argv[2], "JPEG", quality=82, optimize=True)' \
    "$OUT/copilot-cpu-flow.webp" "$OUT/copilot-demo-poster.jpg"
  log "poster copilot-demo-poster.jpg"
}

main() {
  check_inputs
  mkdir -p "$OUT"
  crop_screenshots
  encode_video
  log "done: $(ls "$OUT" | wc -l) files in $OUT"
}
main "$@"
