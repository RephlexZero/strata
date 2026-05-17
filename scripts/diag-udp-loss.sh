#!/usr/bin/env bash
# =============================================================================
# diag-udp-loss.sh  —  DIAGNOSTIC ONLY. Not part of the pipeline. Safe to delete.
# =============================================================================
# Phase-0 isolation harness for claude_burst_fix_plan.md. Sends PLAIN paced UDP
# (zero strata code in the path) from a bound cellular interface to the VPS and
# measures authoritative per-sequence loss / reorder / gap / delay-gradient at
# the VPS. Establishes whether the carrier is intrinsically lossy (T0/T0b),
# whether a burst pattern alone induces loss (T0c), and the per-link bottleneck
# rate + buffer depth (T0s sweep) used to calibrate the BDP cap.
#
# Mirrors the proven field-test.sh pattern: SO_BINDTODEVICE + bind to the
# interface's own IPv4 + a CONNECTED socket (unconnected/0.0.0.0 false-negatives
# because the host default route is wlan0). All SSH/SCP control traffic is
# pinned to wlan0 so it never touches the SIMs under test.
#
# Usage:
#   ./scripts/diag-udp-loss.sh --mode even  --iface enp2s0f0u3 --port 5000 \
#       --bitrate-kbps 1200 --secs 120
#   ./scripts/diag-udp-loss.sh --mode even  --pair enp2s0f0u3:5000 enp11s0f3u1u3:5002 \
#       --bitrate-kbps 600 --secs 120
#   ./scripts/diag-udp-loss.sh --mode burst --pair enp2s0f0u3:5000 enp11s0f3u1u3:5002 \
#       --baseline-kbps 600 --burst-rate-kbps 8000 --burst-secs 0.4 --burst-period 5 --secs 120
#   ./scripts/diag-udp-loss.sh --mode sweep --iface enp2s0f0u3 --port 5000 \
#       --sweep-kbps 500,1000,2000,4000,8000,16000 --stage-secs 15
#
# VPS host: --vps root@HOST  (default: derived from .env STRATA_RECEIVER_HOST,
# else STRATA_DEPLOY_HOST). Requires python3 locally and on the VPS.
# =============================================================================
set -uo pipefail

RED='\033[0;31m'; GRN='\033[0;32m'; YEL='\033[1;33m'; NC='\033[0m'
info() { echo -e "${GRN}[✓]${NC} $*"; }
warn() { echo -e "${YEL}[!]${NC} $*"; }
fail() { echo -e "${RED}[✗]${NC} $*"; exit 1; }

# ---- .env (best-effort, for VPS host + control iface) ----------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
if [[ -f "$REPO_ROOT/.env" ]]; then set -a; . "$REPO_ROOT/.env"; set +a; fi

MODE="even"
declare -a LINKS=()          # entries: "iface:port"
BITRATE_KBPS=1200
BASELINE_KBPS=600
BURST_RATE_KBPS=8000
BURST_SECS=0.4
BURST_PERIOD=5
SECS=120
SWEEP_KBPS="500,1000,2000,4000,8000,16000"
STAGE_SECS=15
PAYLOAD=1200
VPS=""
CTRL_IFACE="${STRATA_DEPLOY_IFACE:-wlan0}"

# ---- arg parse -------------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --mode)          MODE="$2"; shift 2;;
    --iface)         LINKS+=("$2:__PORT__"); PEND_IFACE="$2"; shift 2;;
    --port)          LINKS[-1]="${PEND_IFACE}:$2"; shift 2;;
    --pair)          shift; while [[ $# -gt 0 && "$1" != --* ]]; do LINKS+=("$1"); shift; done;;
    --bitrate-kbps)  BITRATE_KBPS="$2"; shift 2;;
    --baseline-kbps) BASELINE_KBPS="$2"; shift 2;;
    --burst-rate-kbps) BURST_RATE_KBPS="$2"; shift 2;;
    --burst-secs)    BURST_SECS="$2"; shift 2;;
    --burst-period)  BURST_PERIOD="$2"; shift 2;;
    --secs)          SECS="$2"; shift 2;;
    --sweep-kbps)    SWEEP_KBPS="$2"; shift 2;;
    --stage-secs)    STAGE_SECS="$2"; shift 2;;
    --payload)       PAYLOAD="$2"; shift 2;;
    --vps)           VPS="$2"; shift 2;;
    --ctrl-iface)    CTRL_IFACE="$2"; shift 2;;
    -h|--help)       sed -n '2,40p' "$0"; exit 0;;
    *)               fail "unknown arg: $1";;
  esac
done

[[ ${#LINKS[@]} -gt 0 ]] || fail "no links: use --iface/--port or --pair iface:port ..."
command -v python3 >/dev/null || fail "python3 required locally"

if [[ -z "$VPS" ]]; then
  H="${STRATA_RECEIVER_HOST:-${STRATA_DEPLOY_HOST:-}}"
  [[ -n "$H" ]] || fail "no VPS: pass --vps root@HOST (or set STRATA_RECEIVER_HOST in .env)"
  [[ "$H" == *@* ]] && VPS="$H" || VPS="root@${H%%:*}"
fi
VPS_HOST="${VPS##*@}"

# Pin control plane to the WiFi/ctrl iface so it never loads the SIMs.
CTRL_SRC="$(ip -o -4 addr show dev "$CTRL_IFACE" 2>/dev/null | awk '{print $4}' | head -n1 | cut -d/ -f1 || true)"
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=8 -o StrictHostKeyChecking=accept-new)
[[ -n "$CTRL_IFACE" ]] && SSH_OPTS+=(-o "BindInterface=${CTRL_IFACE}")
[[ -n "$CTRL_SRC"  ]] && SSH_OPTS+=(-o "BindAddress=${CTRL_SRC}")

ssh "${SSH_OPTS[@]}" "$VPS" 'command -v python3 >/dev/null' \
  || fail "python3 required on VPS ($VPS)"

TAG="$$"
cleanup() {
  ssh "${SSH_OPTS[@]}" "$VPS" \
    "pkill -f 'strata-diag-rx-${TAG}' 2>/dev/null; rm -f /tmp/strata-diag-rx-${TAG}.* 2>/dev/null" \
    >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

# ---- VPS-side per-sequence counter (clock-skew-immune; uses RELATIVE delay) -
RX_PY=$(mktemp /tmp/strata-diag-rx-local-XXXX.py)
cat > "$RX_PY" <<'PYEOF'
import socket, struct, sys, time, json
port = int(sys.argv[1]); window = float(sys.argv[2]); link = sys.argv[3]
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("0.0.0.0", port)); s.settimeout(window + 5.0)
seen=set(); recv=0; dups=0; reordered=0; max_seq=-1
first=None; last=None; prev_arr=None; max_gap_ms=0.0
# per-stage (sweep): stage -> [count, bytes, sum_reldelay_ns]
stages={}
# relative delay = (arrival_ns - send_ts_ns); only the *minimum* is used as
# baseline so a constant sender/receiver clock offset cancels (drift over a
# ~2min run is negligible). This is a delay GRADIENT, not absolute OWD.
min_rel=None
t_end=time.time()+window+4.0
while time.time()<t_end:
    try:
        d,_=s.recvfrom(2048)
    except socket.timeout:
        break
    if len(d)<24: continue
    seq, send_ts, stage = struct.unpack_from(">QQ", d, 0)[0], \
        struct.unpack_from(">Q", d, 8)[0], struct.unpack_from(">I", d, 16)[0]
    arr=time.time_ns()
    recv+=1
    if first is None: first=arr
    last=arr
    if prev_arr is not None:
        g=(arr-prev_arr)/1e6
        if g>max_gap_ms: max_gap_ms=g
    prev_arr=arr
    rel=arr-send_ts
    if min_rel is None or rel<min_rel: min_rel=rel
    st=stages.setdefault(stage,[0,0,0])
    st[0]+=1; st[1]+=len(d); st[2]+=rel
    if seq in seen: dups+=1
    else:
        seen.add(seq)
        if seq<max_seq: reordered+=1
        if seq>max_seq: max_seq=seq
uniq=len(seen)
total=max_seq+1 if max_seq>=0 else 0
loss=round(100.0*(total-uniq)/total,3) if total>0 else 0.0
dur=((last-first)/1e9) if (first and last and last>first) else window
btl_kbps=round(uniq*len(d)*8/1000.0/dur,1) if recv and dur>0 else 0.0
# sweep knee: first stage whose mean queue-delay (rel-min_rel) exceeds 1.5x the
# lowest-stage baseline; btl ≈ delivered rate of the stage just below the knee;
# buffer depth ≈ max per-stage mean queue-delay (ms).
knee_kbps=0.0; buf_ms=0.0; stage_tbl=[]
if stages:
    base=min_rel if min_rel is not None else 0
    rows=[]
    for stg in sorted(stages):
        c,b,sr=stages[stg]
        qd_ms=((sr/c)-base)/1e6 if c else 0.0
        rows.append((stg,c,round(b*8/1000.0,1),round(qd_ms,1)))
        buf_ms=max(buf_ms,qd_ms)
    stage_tbl=rows
    if rows:
        b0=rows[0][3]
        prev=rows[0]
        for r in rows:
            if r[3] > max(b0*1.5, b0+30.0):
                knee_kbps=prev[2]; break
            prev=r
print(json.dumps({"link":link,"port":port,"recv":recv,"uniq":uniq,
  "max_seq":max_seq,"loss_pct":loss,"reordered":reordered,"dups":dups,
  "max_gap_ms":round(max_gap_ms,1),"btl_kbps":btl_kbps,
  "sweep_knee_kbps":round(knee_kbps,1),"buffer_depth_ms":round(buf_ms,1),
  "stages":stage_tbl}))
PYEOF
RX_REMOTE="/tmp/strata-diag-rx-${TAG}.py"
scp "${SSH_OPTS[@]}" -q "$RX_PY" "${VPS}:${RX_REMOTE}" || fail "scp receiver to VPS failed"
rm -f "$RX_PY"

# total wall the receiver should listen (sweep = stages*stage_secs)
if [[ "$MODE" == "sweep" ]]; then
  IFS=',' read -ra _S <<< "$SWEEP_KBPS"; WINDOW=$(( ${#_S[@]} * STAGE_SECS ))
else WINDOW="$SECS"; fi

# ---- local paced sender (CONNECTED, SO_BINDTODEVICE, iface-IP bound) --------
SND_PY=$(mktemp /tmp/strata-diag-snd-XXXX.py)
cat > "$SND_PY" <<'PYEOF'
import socket, struct, sys, time
mode, src, iface, dst, port = sys.argv[1:6]
port=int(port); payload=int(sys.argv[6]); secs=float(sys.argv[7])
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)
try: s.setsockopt(socket.SOL_SOCKET,25,(iface+"\0").encode())  # SO_BINDTODEVICE
except OSError as e: print(f"SO_BINDTODEVICE failed: {e}",file=sys.stderr); sys.exit(1)
s.bind((src,0)); s.connect((dst,port))
buf=bytearray(payload)
seq=0
def send(stage):
    global seq
    struct.pack_into(">QQ",buf,0,seq,time.time_ns())
    struct.pack_into(">I",buf,16,stage)
    try: s.send(buf)
    except OSError: pass
    seq+=1
def pace(kbps,duration,stage):
    if kbps<=0: time.sleep(duration); return
    iv=payload*8.0/(kbps*1000.0)
    t=time.perf_counter(); end=t+duration; nxt=t
    while time.perf_counter()<end:
        send(stage); nxt+=iv
        d=nxt-time.perf_counter()
        if d>0: time.sleep(d)
if mode=="even":
    pace(int(sys.argv[8]),secs,0)
elif mode=="burst":
    base=int(sys.argv[8]); brate=int(sys.argv[9]); bsec=float(sys.argv[10]); bper=float(sys.argv[11])
    t0=time.perf_counter()
    while time.perf_counter()-t0<secs:
        cyc=min(bper, secs-(time.perf_counter()-t0))
        if cyc<=0: break
        pace(base, max(0.0,cyc-bsec), 0)
        pace(brate, min(bsec,cyc), 1)
elif mode=="sweep":
    stagesecs=float(sys.argv[8]); rates=[int(x) for x in sys.argv[9].split(",")]
    for i,r in enumerate(rates): pace(r,stagesecs,i)
print(f"SENT={seq}")
PYEOF

run_link() {
  local entry="$1" idx="$2"
  local iface="${entry%%:*}" port="${entry##*:}"
  local src; src="$(ip -o -4 addr show dev "$iface" 2>/dev/null | awk '{print $4}' | head -n1 | cut -d/ -f1 || true)"
  [[ -n "$src" ]] || { warn "link $idx: no IPv4 on $iface — skipping"; return; }
  # Start the VPS counter for this port (self-terminating).
  ssh "${SSH_OPTS[@]}" "$VPS" \
    "nohup timeout $((WINDOW+8)) python3 ${RX_REMOTE} ${port} ${WINDOW} ${iface} \
       > /tmp/strata-diag-rx-${TAG}.${port}.json 2>/tmp/strata-diag-rx-${TAG}.${port}.err < /dev/null &" \
    >/dev/null 2>&1
}
read_link() {
  local entry="$1" port="${entry##*:}"
  ssh "${SSH_OPTS[@]}" "$VPS" "cat /tmp/strata-diag-rx-${TAG}.${port}.json 2>/dev/null" 2>/dev/null
}

echo "── diag-udp-loss: mode=$MODE links=${LINKS[*]} vps=$VPS window=${WINDOW}s ──"
for i in "${!LINKS[@]}"; do run_link "${LINKS[$i]}" "$i"; done
sleep 2  # let VPS listeners bind before traffic

declare -a SND_PIDS=()
for entry in "${LINKS[@]}"; do
  iface="${entry%%:*}"; port="${entry##*:}"
  src="$(ip -o -4 addr show dev "$iface" 2>/dev/null | awk '{print $4}' | head -n1 | cut -d/ -f1 || true)"
  [[ -n "$src" ]] || continue
  case "$MODE" in
    even)  python3 "$SND_PY" even  "$src" "$iface" "$VPS_HOST" "$port" "$PAYLOAD" "$SECS" "$BITRATE_KBPS" & ;;
    burst) python3 "$SND_PY" burst "$src" "$iface" "$VPS_HOST" "$port" "$PAYLOAD" "$SECS" \
             "$BASELINE_KBPS" "$BURST_RATE_KBPS" "$BURST_SECS" "$BURST_PERIOD" & ;;
    sweep) python3 "$SND_PY" sweep "$src" "$iface" "$VPS_HOST" "$port" "$PAYLOAD" "$SECS" \
             "$STAGE_SECS" "$SWEEP_KBPS" & ;;
    *) fail "unknown mode: $MODE";;
  esac
  SND_PIDS+=($!)
done
info "sending for ${SECS:-$WINDOW}s ..."
for p in "${SND_PIDS[@]}"; do wait "$p" 2>/dev/null || true; done

sleep 6  # let final datagrams + receiver flush
echo ""
echo "── Results (VPS-side authoritative per-sequence) ──"
for entry in "${LINKS[@]}"; do
  iface="${entry%%:*}"
  j="$(read_link "$entry")"
  if [[ -z "$j" ]]; then warn "link $iface (:${entry##*:}): no result (receiver did not report)"; continue; fi
  echo "  $iface :  $j"
done
rm -f "$SND_PY"
echo ""
echo "Bands: Clean <0.5%  Mild 0.5–3%  Heavy >3% or any max_gap_ms ≥ ~1000"
