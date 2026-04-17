#!/usr/bin/env bash
# Record the emskin splash on sway and encode to mp4.
# Recording starts when emskin launches and stops when it exits.
# Output defaults to <repo>/images/demo.mp4 regardless of CWD.

set -euo pipefail

usage() {
    cat <<EOF
Usage: $(basename "$0") [options] [output.mp4]

Options:
  -h, --help           Show this help
  -w, --workspace NAME Dedicated recording workspace (default: splash-rec)
      --fps N          Output framerate (default: 60)
      --crf N          x264 quality, lower=better (default: 20)
      --no-build       Skip cargo build even if sources are newer

Env:
  EMSKIN_ARGS          Extra args appended to the emskin invocation
                       (default: --standalone --xkb-layout us --xkb-variant dvorak)
EOF
}

die() { echo "error: $*" >&2; exit 1; }

WORKSPACE="splash-rec"
FPS=60
CRF=20
BUILD=1
OUT=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help)       usage; exit 0 ;;
        -w|--workspace)  WORKSPACE="$2"; shift 2 ;;
        --fps)           FPS="$2"; shift 2 ;;
        --crf)           CRF="$2"; shift 2 ;;
        --no-build)      BUILD=0; shift ;;
        -*)              die "unknown option: $1" ;;
        *)               OUT="$1"; shift ;;
    esac
done

# Paths (script is <repo>/scripts/record-splash.sh).
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
# Cargo workspace: target/ lives at the repo root, not inside crates/emskin.
BINARY="$REPO_ROOT/target/debug/emskin"
OUT="${OUT:-$REPO_ROOT/images/demo.mp4}"

# Dependencies.
for cmd in wf-recorder ffmpeg swaymsg jq cargo; do
    command -v "$cmd" >/dev/null || die "missing dependency: $cmd"
done
[[ -n "${SWAYSOCK:-}" ]] || die "SWAYSOCK unset — this script requires sway"

# Temp workspace (auto-cleaned on exit).
WORK=$(mktemp -d -t emskin-rec-XXXX)
RAW="$WORK/raw.mp4"
LOG="$WORK/wf-recorder.log"

REC_PID="" APP_PID="" ORIG_WS=""
cleanup() {
    [[ -n "$REC_PID" ]] && { kill -INT "$REC_PID" 2>/dev/null || true; wait "$REC_PID" 2>/dev/null || true; }
    [[ -n "$APP_PID" ]] && { kill     "$APP_PID" 2>/dev/null || true; wait "$APP_PID" 2>/dev/null || true; }
    [[ -n "$ORIG_WS" ]] && swaymsg workspace "$ORIG_WS" >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
trap cleanup EXIT

# Build if needed. `crates/*/src` covers every workspace member.
if (( BUILD )) && { [[ ! -x "$BINARY" ]] || [[ -n "$(find "$REPO_ROOT/crates" -name '*.rs' -newer "$BINARY" 2>/dev/null | head -1)" ]]; }; then
    echo "Building emskin…"
    (cd "$REPO_ROOT" && cargo build --quiet)
fi
[[ -x "$BINARY" ]] || die "binary not found: $BINARY (try without --no-build)"

# Detect focused workspace + output.
read -r ORIG_WS REC_OUTPUT < <(
    swaymsg -t get_workspaces | jq -r '.[] | select(.focused) | "\(.name) \(.output)"'
)
[[ -n "${ORIG_WS:-}" && -n "${REC_OUTPUT:-}" ]] || die "cannot detect focused sway workspace"

# Switch to a clean workspace, start recorder, launch emskin.
mkdir -p "$(dirname "$OUT")"
swaymsg workspace "$WORKSPACE" >/dev/null
sleep 0.2  # let sway settle before wf-recorder grabs the output

echo "Recording → $OUT  (output=$REC_OUTPUT, ws=$WORKSPACE)"
wf-recorder -o "$REC_OUTPUT" -f "$RAW" 2>"$LOG" &
REC_PID=$!
sleep 0.5  # give wf-recorder time to attach before emskin paints

"$BINARY" ${EMSKIN_ARGS:---standalone --xkb-layout us --xkb-variant dvorak} &>/dev/null &
APP_PID=$!

wait "$APP_PID" 2>/dev/null || true
APP_PID=""

kill -INT "$REC_PID" 2>/dev/null || true
wait "$REC_PID" 2>/dev/null || true
REC_PID=""

swaymsg workspace "$ORIG_WS" >/dev/null

# Sanity check on the raw capture.
[[ -s "$RAW" ]] || { cat "$LOG" >&2; die "wf-recorder produced no output"; }

# Trim the 0.5s lead-in and normalize PTS.
ffmpeg -y -loglevel warning -ss 0.5 -i "$RAW" \
    -c:v libx264 -preset veryfast -crf "$CRF" -r "$FPS" -pix_fmt yuv420p "$OUT"

echo "Done: $OUT ($(du -h "$OUT" | cut -f1))"
