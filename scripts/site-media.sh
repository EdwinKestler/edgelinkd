#!/usr/bin/env bash
# Prepare the project website media in site/media/ from the raw captures in assets/.
#
#   scripts/site-media.sh              # everything
#   scripts/site-media.sh qr           # only the donation QR (also: screenshots, video)
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

DONATION_QR="assets/donations/airtmMe-QR.png"
DONATION_URL="airtm.me/edwin30cg33xl"

# Copy the donation QR at 2x with nearest-neighbour scaling (stays sharp), after checking that it
# still decodes to the donation link printed on the site.
donation_qr() {
  [ -f "$DONATION_QR" ] || { log "missing $DONATION_QR"; exit 1; }
  python3 -I - "$DONATION_QR" "$OUT/donate-airtm-qr.png" "$DONATION_URL" <<'PY'
import sys
import cv2
import numpy as np
from PIL import Image
src, dst, expected = sys.argv[1:4]
im = Image.open(src).convert("RGB")
probe = np.array(im.resize((800, 800), Image.NEAREST))[:, :, ::-1]
probe = cv2.copyMakeBorder(probe, 80, 80, 80, 80, cv2.BORDER_CONSTANT, value=(255, 255, 255))
data, _, _ = cv2.QRCodeDetector().detectAndDecode(probe)
if data.rstrip("/").removeprefix("https://") != expected:
    sys.exit(f"{src} decodes to {data!r}, expected {expected!r}")
im.resize((im.width * 2, im.height * 2), Image.NEAREST).save(dst, optimize=True)
PY
  log "donation QR donate-airtm-qr.png -> $DONATION_URL"
}

# Steps: all (default), or any of: screenshots video qr
main() {
  local steps="${*:-screenshots video qr}"
  mkdir -p "$OUT"
  case " $steps " in *" screenshots "*|*" video "*) check_inputs ;; esac
  case " $steps " in *" screenshots "*) crop_screenshots ;; esac
  case " $steps " in *" video "*) encode_video ;; esac
  case " $steps " in *" qr "*) donation_qr ;; esac
  log "done: $(ls "$OUT" | wc -l) files in $OUT"
}
main "$@"
