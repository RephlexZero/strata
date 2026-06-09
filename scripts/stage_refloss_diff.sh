#!/usr/bin/env bash
# scripts/stage_refloss_diff.sh — localize the "grey / Could not find ref with POC"
# fault by counting H.265 reference-loss errors at EACH pipeline stage and diffing.
#
# This answers the review's key question: "What single diagnostic most cleanly
# separates 'encoder emitting bad frames' from 'network loss corrupting good
# frames' from 'receiver/mux mangling a good stream'?" — and ends the loop of
# fixing contributing factors without confirming the dominant one.
#
# The signature error is the decoder's "Could not find ref with POC" (a missing
# /corrupt reference frame). We decode the SAME stream captured at four points
# and compare the per-stage error counts. The stage where the count first jumps
# is where the corruption is introduced:
#
#   Stage 0  Encoder GOP/frame-type probe   — is the IDR cadence what we assume?
#   Stage 1  Sender pre-transport ES        — does the ENCODER emit clean frames?
#   Stage 2  Receiver post-transport TS     — did the BONDED WIRE / FEC corrupt it?
#            (delivered, BEFORE the DeliveredStream gate / demux)
#   Stage 3  Receiver post-gate HLS segments— did the GATE / RE-MUX mangle it?
#   Stage 4  YouTube Live output            — did YOUTUBE'S transcoder add grey?
#
# Interpretation (see analyze() at the bottom for the printed guide):
#   S1 dirty                      -> encoder is emitting bad frames (or live
#                                    bitrate reconfig glitch). Look at codec.rs.
#   S1 clean, S2 dirty            -> transport/FEC is dropping reference packets.
#                                    Look at FEC K/R, interleaving, link shedding.
#   S1+S2 clean, S3 dirty         -> the receiver gate/re-mux passes corrupt AUs
#                                    or mis-handles DISCONT. Look at the gate +
#                                    aggregator DISCONT granularity.
#   S1..S3 clean, S4 dirty        -> YouTube transcoder amplification (untagged
#                                    discontinuities / A-V desync / GOP-wide
#                                    re-decode of sparse ref-loss).
#   S2 sparse, S4 "throughout"    -> sparse in-bitstream ref-loss amplified by
#                                    YouTube's re-encode (the most likely case
#                                    per the architecture review).
#
# Run the stages where the relevant host is (0/1 on the Orange Pi, 2/3 on the
# VPS, 4 anywhere with network). Each writes a .ts/.m3u8 + a .refloss count file
# into $OUTDIR; run `analyze` last (anywhere) to tabulate whatever is present.
#
# Usage:
#   OUTDIR=/tmp/refloss ./scripts/stage_refloss_diff.sh <stage> [args]
#     stage0_encoder            [DEVICE] [RES] [FPS] [SECS]     # on the Orange Pi
#     stage1_sender_es          [DEVICE] [RES] [FPS] [SECS]     # on the Orange Pi
#     stage2_delivered_ts       <links> [SECS]                  # on the VPS
#     stage3_postgate_hls       <links> <relay-url> [SECS]      # on the VPS
#     stage4_youtube            <youtube-watch-or-m3u8-url> [SECS]
#     analyze                                                   # anywhere
#
# `links` is the stratasrc bind list, e.g. "0.0.0.0:5000,0.0.0.0:5002".
# Requires: ffmpeg/ffprobe; gst-launch-1.0 (stages 0/1); the strata-pipeline
# binary on PATH or $STRATA_BIN (stages 2/3); yt-dlp (stage 4, optional).

set -euo pipefail

OUTDIR="${OUTDIR:-/tmp/strata-refloss}"
STRATA_BIN="${STRATA_BIN:-strata-pipeline}"
CODEC="${STRATA_CODEC:-h265}"
mkdir -p "$OUTDIR"

# Count "Could not find ref with POC" (and sibling concealment) errors in a
# decodable file. -err_detect explode surfaces reference errors instead of
# silently concealing them, so the count reflects real reference loss.
count_refloss() {
  local f="$1" label="$2"
  if [[ ! -s "$f" ]]; then
    echo "  [$label] MISSING/empty: $f"
    return
  fi
  local log="$OUTDIR/${label}.decode.log"
  ffmpeg -hide_banner -v error -err_detect explode -i "$f" -f null - 2>"$log" || true
  local poc total
  poc=$(grep -c "Could not find ref" "$log" 2>/dev/null || echo 0)
  total=$(wc -l <"$log" 2>/dev/null | tr -d ' ')
  echo "$poc" >"$OUTDIR/${label}.refloss"
  echo "$total" >"$OUTDIR/${label}.errtotal"
  echo "  [$label] ref-loss(POC)=$poc  total-decode-errors=$total  ($f)"
}

enc_chain() {
  # Mirror the sender's encode chain closely enough to characterize the HW path.
  # Auto-picks the Rockchip encoder if present, else falls back to x265.
  if gst-inspect-1.0 mpph265enc >/dev/null 2>&1; then echo "mpph265enc name=enc"
  elif gst-inspect-1.0 rkmpph265enc >/dev/null 2>&1; then echo "rkmpph265enc name=enc"
  else echo "x265enc name=enc tune=4 speed-preset=ultrafast key-int-max=30"; fi
}

stage0_encoder() {
  local dev="${1:-/dev/video0}" res="${2:-1280x720}" fps="${3:-30}" secs="${4:-15}"
  local w="${res%x*}" h="${res#*x}"
  local raw="$OUTDIR/stage0.h265"
  echo "== Stage 0: encoder GOP / frame-type probe (dev=$dev ${res}@${fps} ${secs}s) =="
  timeout "$((secs+5))" gst-launch-1.0 -e \
    v4l2src device="$dev" num-buffers="$((fps*secs))" \
    ! decodebin ! videoconvert ! videorate ! videoscale \
    ! "video/x-raw,width=$w,height=$h,framerate=$fps/1" \
    ! videoconvert ! "video/x-raw,format=NV12" \
    ! $(enc_chain) ! h265parse config-interval=-1 \
    ! "video/x-h265,stream-format=byte-stream" \
    ! filesink location="$raw" 2>"$OUTDIR/stage0.gst.log" || true
  echo "-- frame types (I/P) and IDR spacing:"
  ffprobe -hide_banner -v error -select_streams v -show_frames \
    -show_entries frame=pict_type,key_frame -of csv "$raw" 2>/dev/null \
    | awk -F, '{n++; if($3=="I"||$2==1){if(last)print "   IDR gap = " n-last " frames"; last=n}} END{print "   total frames = " n}'
  echo "   (expect ~$fps-frame IDR spacing == ~1s, closed-GOP I-frames; if much"
  echo "    larger or no recurring I, the GOP assumption is wrong — set gop=$fps)"
  count_refloss "$raw" "stage1_es"   # the local encode IS the clean-encoder baseline
}

stage1_sender_es() {
  # Alias: capturing the production sender's pre-transport ES requires a tee in
  # the pipeline (not present today). The standalone encode in stage0 is the
  # representative clean-encoder baseline and is written as stage1_es there.
  stage0_encoder "$@"
}

stage2_delivered_ts() {
  local links="${1:?need stratasrc links, e.g. 0.0.0.0:5000,0.0.0.0:5002}" secs="${2:-60}"
  local ts="$OUTDIR/stage2_delivered.ts"
  echo "== Stage 2: receiver post-transport TS (pre-gate), ${secs}s =="
  echo "   Capturing the DELIVERED wire stream (stratasrc --output, before the"
  echo "   DeliveredStream gate and demux). Start the sender now."
  timeout "$secs" "$STRATA_BIN" receiver --links "$links" --output "$ts" \
    2>"$OUTDIR/stage2.log" || true
  count_refloss "$ts" "stage2_delivered"
}

stage3_postgate_hls() {
  local links="${1:?need links}" relay="${2:?need --relay-url}" secs="${3:-60}"
  echo "== Stage 3: receiver post-gate HLS segments, ${secs}s =="
  echo "   Running the real HLS relay; segments land in a tmpfs temp dir that the"
  echo "   receiver prints as 'HLS temp dir: ...'. We copy + concat them."
  : >"$OUTDIR/stage3.log"
  timeout "$secs" "$STRATA_BIN" receiver --links "$links" \
    --relay-url "$relay" --relay-type hls 2>"$OUTDIR/stage3.log" || true
  local dir
  dir=$(grep -oE 'HLS temp dir: [^ ]+' "$OUTDIR/stage3.log" | head -1 | awk '{print $4}')
  if [[ -n "${dir:-}" && -d "$dir" ]]; then
    cat "$dir"/segment*.ts >"$OUTDIR/stage3_postgate.ts" 2>/dev/null || true
    cp "$dir"/playlist.m3u8 "$OUTDIR/stage3_postgate.m3u8" 2>/dev/null || true
    echo "   collected $(ls "$dir"/segment*.ts 2>/dev/null | wc -l) segments from $dir"
    grep -c "EXT-X-DISCONTINUITY" "$OUTDIR/stage3_postgate.m3u8" 2>/dev/null \
      | xargs -I{} echo "   EXT-X-DISCONTINUITY tags in playlist: {} (0 == untagged gaps -> YouTube grey risk)"
  else
    echo "   WARN: could not find HLS temp dir in stage3.log"
  fi
  count_refloss "$OUTDIR/stage3_postgate.ts" "stage3_postgate"
}

stage4_youtube() {
  local url="${1:?need YouTube watch URL or .m3u8}" secs="${2:-60}"
  local ts="$OUTDIR/stage4_youtube.ts"
  echo "== Stage 4: YouTube Live output, ${secs}s =="
  if command -v yt-dlp >/dev/null 2>&1; then
    timeout "$((secs+15))" yt-dlp --live-from-start --downloader ffmpeg \
      --downloader-args "ffmpeg_i:-t $secs" -f "bv*+ba/b" -o "$ts" "$url" \
      2>"$OUTDIR/stage4.log" || \
    ffmpeg -hide_banner -v error -t "$secs" -i "$url" -c copy "$ts" 2>>"$OUTDIR/stage4.log" || true
  else
    echo "   yt-dlp not found; trying ffmpeg directly on the URL"
    ffmpeg -hide_banner -v error -t "$secs" -i "$url" -c copy "$ts" 2>"$OUTDIR/stage4.log" || true
  fi
  count_refloss "$ts" "stage4_youtube"
}

analyze() {
  echo "================ STAGED REF-LOSS DIFF ($OUTDIR) ================"
  printf "%-22s %12s %16s\n" "stage" "ref-loss(POC)" "total-dec-errors"
  for s in stage1_es stage2_delivered stage3_postgate stage4_youtube; do
    local poc tot
    poc=$(cat "$OUTDIR/${s}.refloss" 2>/dev/null || echo "-")
    tot=$(cat "$OUTDIR/${s}.errtotal" 2>/dev/null || echo "-")
    printf "%-22s %12s %16s\n" "$s" "$poc" "$tot"
  done
  echo "---------------------------------------------------------------"
  echo "INTERPRETATION:"
  echo "  * stage1 dirty            -> ENCODER emits bad frames / live-reconfig glitch."
  echo "  * stage1 clean, stage2 dirty -> TRANSPORT/FEC drops reference packets"
  echo "       (K=32/R=4, no interleaving: a >=5-pkt burst kills a generation)."
  echo "  * stage2 clean, stage3 dirty -> GATE/RE-MUX passes corrupt AUs"
  echo "       (DISCONT is byte-granular, decoupled from the holed access unit)."
  echo "  * stage3 clean/sparse, stage4 'throughout' -> YOUTUBE amplifies sparse"
  echo "       in-bitstream ref-loss via GOP-wide re-decode + untagged HLS gaps."
  echo "  Also check: EXT-X-DISCONTINUITY count in stage3_postgate.m3u8 (0 == the"
  echo "  gate drops GOPs but never tags the gap -> YouTube bridges it with grey)."
  echo "==============================================================="
}

cmd="${1:-analyze}"; shift || true
case "$cmd" in
  stage0_encoder)     stage0_encoder "$@" ;;
  stage1_sender_es)   stage1_sender_es "$@" ;;
  stage2_delivered_ts) stage2_delivered_ts "$@" ;;
  stage3_postgate_hls) stage3_postgate_hls "$@" ;;
  stage4_youtube)     stage4_youtube "$@" ;;
  analyze)            analyze ;;
  *) echo "unknown stage '$cmd'; see header for usage" >&2; exit 1 ;;
esac
