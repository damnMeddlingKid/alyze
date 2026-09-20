#!/usr/bin/env bash
#
# Record Instruments traces for one criterion benchmark.
#
# Produces three traces, because they answer different questions:
#   * CPU Profiler    — where the time goes (samples + stacks, needs debug symbols)
#   * CPU Counters    — why it is slow (stalls, mispredicts, dependency chains)
#   * Processor Trace — which exact branches mispredict (huge; short duration)
#
# Usage:
#   scripts/profile-bench.sh 'word break/windowed'
#   scripts/profile-bench.sh --time 30 --bench wikipedia 'word break/dfa'
#
set -euo pipefail

BENCH_TARGET="wikipedia"
PROFILE_TIME="20"
PT_TIME="5"
OUT_DIR="target/traces"
DRY_RUN=0
FILTER=""

usage() {
    sed -n '3,12p' "$0" | sed 's/^# \{0,1\}//'
    cat <<'EOF'

Arguments:
  <filter>          criterion benchmark filter, e.g. 'word break/windowed'
                    (matched as a regex against the full benchmark id)

Options:
  --bench <name>    cargo bench target        (default: wikipedia)
  --time <secs>     seconds per sampling trace(default: 20)
  --pt-time <secs>  seconds for Processor Trace (default: 5, it is data-heavy)
  --out <dir>       where to write .trace     (default: target/traces)
  --dry-run         print the commands only
  -h, --help        this message
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --bench)   BENCH_TARGET="$2"; shift 2 ;;
        --time)    PROFILE_TIME="$2"; shift 2 ;;
        --pt-time) PT_TIME="$2";      shift 2 ;;
        --out)     OUT_DIR="$2";      shift 2 ;;
        --dry-run) DRY_RUN=1;         shift   ;;
        -h|--help) usage; exit 0 ;;
        -*)        echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
        *)         FILTER="$1";       shift   ;;
    esac
done

if [[ -z "$FILTER" ]]; then
    echo "error: no benchmark filter given" >&2
    usage >&2
    exit 1
fi

cd "$(git rev-parse --show-toplevel)"

# Instruments can only map samples back to source when the bench profile carries debug info.
if ! awk '/^\[profile\.bench\]/{f=1;next} /^\[/{f=0} f&&/^[[:space:]]*debug/{found=1} END{exit !found}' Cargo.toml; then
    echo "warning: no 'debug' setting under [profile.bench] in Cargo.toml." >&2
    echo "         Traces will have symbols but no line numbers. Add:" >&2
    echo "             [profile.bench]" >&2
    echo "             debug = true" >&2
    echo >&2
fi

# Build without running, and ask cargo which binary it produced rather than guessing the hash.
echo "==> building bench target '$BENCH_TARGET'"
EXE="$(cargo bench --bench "$BENCH_TARGET" --no-run --message-format=json 2>/dev/null \
    | jq -r --arg t "$BENCH_TARGET" '
        select(.reason == "compiler-artifact")
        | select(.executable != null)
        | select(.target.name == $t)
        | select(.target.kind | index("bench"))
        | .executable' \
    | tail -1)"

if [[ -z "$EXE" || ! -x "$EXE" ]]; then
    echo "error: could not locate the built bench binary for '$BENCH_TARGET'" >&2
    exit 1
fi
echo "    $EXE"

# Instruments refuses to attach to a binary without `com.apple.security.get-task-allow`, and
# cargo-built binaries are unsigned. Sign ad-hoc (`-s -`) with just that entitlement. This has
# to happen after every build, since a rebuild replaces the binary and drops the signature.
echo "==> signing for profiling (get-task-allow)"
ENTITLEMENTS="$(mktemp -t get-task-allow).plist"
trap 'rm -f "$ENTITLEMENTS"' EXIT
cat > "$ENTITLEMENTS" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.get-task-allow</key>
    <true/>
</dict>
</plist>
PLIST
codesign --sign - --force --entitlements "$ENTITLEMENTS" "$EXE"

mkdir -p "$OUT_DIR"
SLUG="$(printf '%s' "$FILTER" | tr -cs '[:alnum:]' '-' | sed 's/^-*//; s/-*$//')"

record() {
    local template="$1" suffix="$2" seconds="$3"
    local out="$OUT_DIR/${SLUG}-${suffix}.trace"

    echo "==> recording '$template' for ${seconds}s -> $out"
    # xctrace refuses to overwrite an existing trace.
    rm -rf "$out"

    local cmd=(
        xctrace record
        --template "$template"
        --output "$out"
        --no-prompt
        --launch -- "$EXE" --bench --profile-time "$seconds" "$FILTER"
    )

    if [[ "$DRY_RUN" == 1 ]]; then
        printf '    '; printf '%q ' "${cmd[@]}"; printf '\n'
    else
        "${cmd[@]}"
    fi
}

# --profile-time is criterion's "just loop, no statistics" mode: without it the trace is
# dominated by criterion's own sampling and outlier analysis rather than the code under test.
record "CPU Profiler" "profiler" "$PROFILE_TIME"
record "CPU Counters" "counters" "$PROFILE_TIME"

# Processor Trace reconstructs exact instruction-level control flow, so it names the individual
# mispredicted branches instead of sampling near them. It also produces orders of magnitude more
# data than the sampling instruments, hence its own much shorter default duration.
record "Processor Trace" "pt" "$PT_TIME"

echo
echo "==> done"
echo "    open $OUT_DIR/${SLUG}-profiler.trace   # where the time goes"
echo "    open $OUT_DIR/${SLUG}-counters.trace   # why it is slow"
echo "    open $OUT_DIR/${SLUG}-pt.trace         # which branches mispredict"
echo
echo "    Note: the first ~second of each trace is process startup and corpus loading."
echo "    Drag-select the steady-state region before reading the call tree or remarks."
