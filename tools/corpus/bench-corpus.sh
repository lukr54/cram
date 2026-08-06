#!/usr/bin/env bash
# The official cram benchmark: cram against 7-Zip and WinRAR on the public corpus.
#
# Usage: bench-corpus.sh <corpus-dir> <work-dir> [rounds]
# Env:   CRAM, SEVENZ, RAR (binaries), MEMCAP (default 20G)
#
# Every rule here is one that was got wrong at least once while this was being built, so each is
# written down rather than left implicit in a flag:
#
#   * RAR gets `-s`. 7-Zip is solid by default and RAR is not, and measuring RAR without it costs
#     it 9.9% of its ratio for a reason that has nothing to do with RAR.
#   * Both competitors get every thread explicitly, so no tool is quietly measured on fewer cores.
#   * Every tool is measured at both ends of its own range. Comparing our maximum against their
#     default is the standard way these tables mislead.
#   * `sync` is inside the timed region for extraction. Without it an extraction stops the clock
#     with gigabytes still in the page cache and whatever runs next pays for the writes -- which is
#     how an earlier run had 7-Zip "extracting" five times faster than it could verify.
#   * Extraction is measured to disk and to tmpfs. The first is the real-world number; the second
#     removes the write wall and leaves the decoder. They answer different questions.
#   * Headline rows run several rounds with the tool order rotated. Single samples on this corpus
#     were wrong by 2x twice.
#   * Judge on the artifact, never the exit code: 7-Zip returns 1 for a warning while writing a
#     perfectly good archive, and every extraction is counted against the corpus afterwards.
#
# Two things exist because the machine running this died mid-run once:
#
#   * every tool runs under a memory cap with swap denied, so a tool that wants more than the box
#     has is killed and recorded as "needed more than the cap" instead of taking the host down. A
#     cap needs MemorySwapMax=0 to bite; with swap available the kernel reclaims instead of
#     failing, which is the machine grinding to a halt rather than a measurement.
#   * results are flushed after every row and a re-run skips rows already recorded, so a crash
#     costs one cell and not an afternoon.
set -u
CORPUS="${1:?usage: bench-corpus.sh <corpus-dir> <work-dir> [rounds]}"
WORK="${2:?usage: bench-corpus.sh <corpus-dir> <work-dir> [rounds]}"
ROUNDS="${3:-3}"

CRAM="${CRAM:-cram}"
SEVENZ="${SEVENZ:-7zz}"
RAR="${RAR:-rar}"
MEMCAP="${MEMCAP:-20G}"
THREADS="$(nproc)"

mkdir -p "$WORK"
RES="$WORK/results.tsv"
LOG="$WORK/bench.log"
[ -f "$RES" ] || printf 'phase\ttool\tmode\tround\tseconds\tcpu_pct\tarchive_bytes\tratio\tpeak_rss_kb\tnote\n' > "$RES"
log() { echo "$*" | tee -a "$LOG"; }
row() { printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$@" >> "$RES"; sync "$RES" 2>/dev/null || sync; }
done_already() { awk -F'\t' -v a="$1" -v b="$2" -v c="$3" -v d="$4" \
  'NR>1 && $1==a && $2==b && $3==c && $4==d {found=1} END{exit !found}' "$RES"; }

# Memory cap, if this system can enforce one. A cap that silently does nothing is worse than none,
# so it is tested rather than assumed.
CAPPED=no
if command -v systemd-run >/dev/null 2>&1; then
  if systemd-run --user --quiet --wait --pipe -p MemoryMax=64M -p MemorySwapMax=0 -- \
       python3 -c 'b=bytearray()
for i in range(40): b.extend(bytearray(10*1024*1024))' >/dev/null 2>&1; then
    CAPPED=no      # it allocated 400 MB under a 64 MB cap: not enforcing
  else
    CAPPED=yes
  fi
fi
# `--working-directory` is not optional and is not a tidiness flag. A transient user unit starts in
# the user's home, NOT in the directory the calling shell is standing in, so `cd "$CORPUS" && cap
# cram a out.cram .` archives $HOME. That is not hypothetical: it ran, and it wrote 25 GB of the
# wrong tree before anyone noticed the clock was wrong.
cap() {
  if [ "$CAPPED" = yes ]; then
    systemd-run --user --quiet --wait --pipe --working-directory="$PWD" \
      -p MemoryMax="$MEMCAP" -p MemorySwapMax=0 -- "$@"
  else
    "$@"
  fi
}

# Prove it, rather than trust the flag. Everything downstream archives `.` and would silently
# capture the wrong tree if this were ever wrong again.
if [ "$CAPPED" = yes ]; then
  seen=$( cd "$CORPUS" && cap pwd 2>/dev/null )
  want=$( cd "$CORPUS" && pwd )
  if [ "$seen" != "$want" ]; then
    echo "ABORT: capped commands run in '$seen', not '$want'." >&2
    echo "Archiving '.' from there would capture the wrong tree. Refusing to run." >&2
    exit 1
  fi
fi

IN_FILES=$(find "$CORPUS" -type f | wc -l)
IN_BYTES=$(find "$CORPUS" -type f -printf '%s\n' | awk '{s+=$1} END{print s}')

log "=== cram benchmark, public corpus ==="
log "corpus:  $CORPUS"
log "         $IN_BYTES bytes, $IN_FILES files"
[ -f "$CORPUS/CORPUS.id" ] && log "         corpus id $(cat "$CORPUS/CORPUS.id")"
log "host:    $(hostname), $THREADS threads, $(free -g | awk '/^Mem:/{print $2}') GB RAM"
log "cap:     $([ "$CAPPED" = yes ] && echo "$MEMCAP, swap denied" || echo "NONE -- this system cannot enforce one")"
log "cram:    $($CRAM --version 2>&1 | head -1)"
log "7-Zip:   $($SEVENZ 2>&1 | sed -n 2p | cut -c1-40)"
log "RAR:     $($RAR 2>&1 | sed -n 2p | cut -c1-40)"
log ""

warm() { find "$CORPUS" -type f -print0 | xargs -0 cat > /dev/null 2>&1; }

create() {
  local tool="$1" mode="$2" round="$3" ext="$4"; shift 4
  done_already create "$tool" "$mode" "$round" && { log "$(printf '%-5s %-8s r%s  (already recorded)' "$tool" "$mode" "$round")"; return; }
  local A="$WORK/out.$ext" t="$WORK/.t"
  rm -f "$A"; : > "$t"
  warm
  ( cd "$CORPUS" && cap /usr/bin/time -f '%e %P %M' -o "$t" "$@" ) >/dev/null 2>>"$LOG"
  local rc=$?
  # Judge on the artifact -- but "non-empty" is not enough. A tool killed by the memory cap can
  # still have written a 32-byte container header, which passes `-s` and then records a row with a
  # blank time and a ratio of 0.0000 as though it were a result. Require a plausible archive AND a
  # timing that parses as a number; `/usr/bin/time` writes prose ("Command terminated by signal 9")
  # exactly when the run is the one you must not record.
  local ob_now=0 timed_ok=no
  [ -f "$A" ] && ob_now=$(stat -c %s "$A" 2>/dev/null || echo 0)
  awk 'NF>=3 && $1+0>0 {ok=1} END{exit !ok}' "$t" 2>/dev/null && timed_ok=yes
  if [ "$ob_now" -lt $((IN_BYTES / 100)) ] || [ "$timed_ok" = no ]; then
    local why; why=$(tr '\n' ' ' < "$t" 2>/dev/null)
    local note="FAILED rc=$rc ${ob_now}B ${why}"
    [ "$CAPPED" = yes ] && note="$note (cap $MEMCAP)"
    log "$(printf '%-5s %-8s r%s  FAILED rc=%s  %s' "$tool" "$mode" "$round" "$rc" "$why")"
    row create "$tool" "$mode" "$round" "" "" "" "" "" "$note"
    rm -f "$A"; return
  fi
  local cs cpu rss ob ratio
  cs=$(awk 'NF>=3{print $1}' "$t" | tail -1); cpu=$(awk 'NF>=3{print $2}' "$t" | tail -1)
  rss=$(awk 'NF>=3{print $3}' "$t" | tail -1)
  ob=$(stat -c %s "$A")
  ratio=$(awk -v o="$ob" -v i="$IN_BYTES" 'BEGIN{printf "%.4f", o/i}')
  log "$(printf '%-5s %-8s r%s  %9ss  cpu %-7s %13s bytes  %s  %6s MB' \
        "$tool" "$mode" "$round" "$cs" "$cpu" "$ob" "$ratio" "$((rss / 1024))")"
  row create "$tool" "$mode" "$round" "$cs" "$cpu" "$ob" "$ratio" "$rss" ""
  rm -f "$A"
}

log "--- create: default settings, $ROUNDS rounds, order rotated ---"
for r in $(seq 1 "$ROUNDS"); do
  case $((r % 3)) in
    1) order="cram 7z rar" ;;
    2) order="7z rar cram" ;;
    *) order="rar cram 7z" ;;
  esac
  for tool in $order; do
    case $tool in
      cram) create cram default "$r" cram $CRAM a "$WORK/out.cram" . -y ;;
      7z)   create 7z   mx5     "$r" 7z   $SEVENZ a "$WORK/out.7z" . -mmt="$THREADS" -mx=5 ;;
      rar)  create rar  s_m3    "$r" rar  $RAR a -mt"$THREADS" -s -m3 -r "$WORK/out.rar" . ;;
    esac
  done
done

log ""
log "--- create: the rest of each tool's range, $ROUNDS rounds ---"
# Ratios here are deterministic, so one run would settle the size column. The timings are not, and
# quoting a one-sample time next to a three-sample one invites exactly the comparison it cannot
# support -- so every row gets the same treatment as the headline.
for r in $(seq 1 "$ROUNDS"); do
  create cram fast  "$r" cram $CRAM a "$WORK/out.cram" . --fast -y
  create cram store "$r" cram $CRAM a "$WORK/out.cram" . --store -y
  create 7z   mx0   "$r" 7z   $SEVENZ a "$WORK/out.7z" . -mmt="$THREADS" -mx=0
  create rar  s_m0  "$r" rar  $RAR a -mt"$THREADS" -s -m0 -r "$WORK/out.rar" .
  create cram small "$r" cram $CRAM a "$WORK/out.cram" . --small -y
  create 7z   mx9   "$r" 7z   $SEVENZ a "$WORK/out.7z" . -mmt="$THREADS" -mx=9
  create rar  s_m5  "$r" rar  $RAR a -mt"$THREADS" -s -m5 -r "$WORK/out.rar" .
done

log ""
log "--- building one archive per tool at default settings, to extract from ---"
for spec in "cram:$CRAM a $WORK/keep.cram . -y" \
            "7z:$SEVENZ a $WORK/keep.7z . -mmt=$THREADS -mx=5" \
            "rar:$RAR a -mt$THREADS -s -m3 -r $WORK/keep.rar ."; do
  t="${spec%%:*}"; cmdline="${spec#*:}"
  [ -s "$WORK/keep.$t" ] && continue
  # shellcheck disable=SC2086
  ( cd "$CORPUS" && cap $cmdline ) >/dev/null 2>>"$LOG"
done
sync
for f in "$WORK"/keep.*; do log "      $(basename "$f") $(stat -c %s "$f") bytes"; done

extract() {
  local tool="$1" round="$2" dest="$3" tag="$4"
  done_already extract "$tool" "$tag" "$round" && { log "$(printf '%-5s %-8s r%s  (already recorded)' "$tool" "$tag" "$round")"; return; }
  local X="$dest/x-$tool" t="$WORK/.t"
  rm -rf "$X"; mkdir -p "$X"
  sync
  case "$tool" in
    cram) cap /usr/bin/time -f '%e %P' -o "$t" sh -c "$CRAM x '$WORK/keep.cram' -o '$X' >/dev/null 2>&1; sync" ;;
    7z)   cap /usr/bin/time -f '%e %P' -o "$t" sh -c "$SEVENZ x '$WORK/keep.7z' -o'$X' -y >/dev/null 2>&1; sync" ;;
    rar)  cap /usr/bin/time -f '%e %P' -o "$t" sh -c "$RAR x -y '$WORK/keep.rar' '$X/' >/dev/null 2>&1; sync" ;;
  esac
  local xs cpu gf gb note
  xs=$(awk 'NF>=2{print $1}' "$t" | tail -1); cpu=$(awk 'NF>=2{print $2}' "$t" | tail -1)
  gf=$(find "$X" -type f | wc -l)
  gb=$(find "$X" -type f -printf '%s\n' | awk '{s+=$1} END{print s+0}')
  note=complete
  { [ "$gf" -ne "$IN_FILES" ] || [ "$gb" -ne "$IN_BYTES" ]; } && note="SHORT $gf/$IN_FILES files"
  log "$(printf '%-5s %-8s r%s  %9ss  cpu %-7s %6s files  %s' "$tool" "$tag" "$round" "$xs" "$cpu" "$gf" "$note")"
  row extract "$tool" "$tag" "$round" "$xs" "$cpu" "" "" "" "$note"
  rm -rf "$X"
}

log ""
log "--- extract to disk, writeback inside the timer ---"
mkdir -p "$WORK/disk"
for r in $(seq 1 "$ROUNDS"); do
  case $((r % 3)) in 1) order="cram 7z rar" ;; 2) order="7z rar cram" ;; *) order="rar cram 7z" ;; esac
  for tool in $order; do extract "$tool" "$r" "$WORK/disk" disk; done
done

SHM_FREE=$(df -k --output=avail /dev/shm 2>/dev/null | tail -1)
if [ -n "${SHM_FREE:-}" ] && [ "$SHM_FREE" -gt $((IN_BYTES / 1024 + 1048576)) ]; then
  log ""
  log "--- extract to tmpfs: the decoder with no write wall (not a real-world number) ---"
  mkdir -p /dev/shm/crambench
  for r in $(seq 1 "$ROUNDS"); do
    case $((r % 3)) in 1) order="cram 7z rar" ;; 2) order="7z rar cram" ;; *) order="rar cram 7z" ;; esac
    for tool in $order; do extract "$tool" "$r" /dev/shm/crambench tmpfs; done
  done
  rm -rf /dev/shm/crambench
else
  log ""
  log "--- tmpfs extract skipped: /dev/shm too small ---"
fi

rm -f "$WORK"/keep.*
rmdir "$WORK/disk" 2>/dev/null || true

log ""
log "--- medians ---"
awk -F'\t' 'NR>1 && $5!="" {k=sprintf("%-8s %-5s %-8s",$1,$2,$3); v[k]=v[k]" "$5}
  END{for(k in v){n=split(v[k],a," ");
    for(i=1;i<n;i++)for(j=i+1;j<=n;j++)if(a[i]+0>a[j]+0){t=a[i];a[i]=a[j];a[j]=t}
    printf "%s n=%d  min %8.2f  median %8.2f  max %8.2f\n", k, n, a[1], a[int((n+1)/2)], a[n]}}' "$RES" \
  | sort | tee -a "$LOG"

log ""
log "=== done -- $RES ==="
