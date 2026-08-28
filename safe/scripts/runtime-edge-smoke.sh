#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
SAFE_BIN=${SAFE_BIN:-$REPO_ROOT/target/debug/safe}
SAFECTL_BIN=${SAFECTL_BIN:-$REPO_ROOT/target/debug/safectl}
KEEP_TMP=${KEEP_TMP:-0}
BUILD=1

usage() {
  printf 'Usage: %s [--no-build] [--keep]\n' "$0"
  printf '  --no-build  Use SAFE_BIN or target/debug/safe without building\n'
  printf '  --keep      Preserve the temporary runtime directory\n'
}

while (($#)); do
  case "$1" in
    --no-build) BUILD=0 ;;
    --keep) KEEP_TMP=1 ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
  shift
done

for dependency in bash jq; do
  command -v "$dependency" >/dev/null || {
    printf 'missing dependency: %s\n' "$dependency" >&2
    exit 1
  }
done

if ((BUILD)); then
  cargo build -p safe -p safectl --manifest-path "$REPO_ROOT/Cargo.toml"
fi
[[ -x "$SAFE_BIN" ]] || {
  printf 'SAFE binary is not executable: %s\n' "$SAFE_BIN" >&2
  exit 1
}
[[ -x "$SAFECTL_BIN" ]] || {
  printf 'safectl binary is not executable: %s\n' "$SAFECTL_BIN" >&2
  exit 1
}

# macOS limits Unix-domain socket paths to roughly 104 bytes. TMPDIR is usually
# much longer than /tmp there, so keep the generated safectl.sock path short.
EDGE_TMPDIR=${SAFE_EDGE_TMPDIR:-/tmp}
EDGE_TMPDIR=${EDGE_TMPDIR%/}
TMP_ROOT=$(mktemp -d "$EDGE_TMPDIR/safe-edges.XXXXXX")
ACTIVE_PIDS=()

cleanup() {
  local pid
  for pid in "${ACTIVE_PIDS[@]:-}"; do
    if kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  if [[ "$KEEP_TMP" == 1 ]]; then
    printf 'Preserved test runtime: %s\n' "$TMP_ROOT"
  else
    rm -rf -- "$TMP_ROOT"
  fi
}
trap cleanup EXIT INT TERM

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  local log
  for log in "$TMP_ROOT"/*/safe-process.log*; do
    if [[ -f "$log" ]]; then
      printf '\n--- %s ---\n' "$log" >&2
      tail -n 100 "$log" >&2
    fi
  done
  KEEP_TMP=1
  exit 1
}

pass() {
  printf 'PASS: %s\n' "$*"
}

start_safe() {
  local base=$1
  local log=$2
  SAFE_RUNTIME_CONFIG="$base/safe.yaml" \
    SAFE_AUTONOMY_MODE_CONFIG_PATH="$base/modes.json" \
    SAFE_SANDBOX_ISOLATION=disabled \
    "$SAFE_BIN" >"$log" 2>&1 &
  LAST_PID=$!
  ACTIVE_PIDS+=("$LAST_PID")
}

stop_safe() {
  local pid=$1
  if kill -0 "$pid" 2>/dev/null; then
    kill "$pid"
    wait "$pid" 2>/dev/null || true
  fi
}

wait_for_path() {
  local path=$1
  local timeout=${2:-10}
  local attempts=$((timeout * 40))
  local i
  for ((i = 0; i < attempts; i++)); do
    [[ -e "$path" ]] && return 0
    if [[ -n "${LAST_PID:-}" ]] && ! kill -0 "$LAST_PID" 2>/dev/null; then
      fail "SAFE exited before creating $path"
    fi
    sleep 0.025
  done
  fail "timed out waiting for $path"
}

wait_for_json() {
  local path=$1
  local filter=$2
  local timeout=${3:-10}
  local attempts=$((timeout * 40))
  local i
  for ((i = 0; i < attempts; i++)); do
    if [[ -s "$path" ]] && jq -e "$filter" "$path" >/dev/null 2>&1; then
      return 0
    fi
    if [[ -n "${LAST_PID:-}" ]] && ! kill -0 "$LAST_PID" 2>/dev/null; then
      fail "SAFE exited while waiting for '$filter' in $path"
    fi
    sleep 0.025
  done
  fail "timed out waiting for jq filter '$filter' in $path"
}

wait_for_text() {
  local path=$1
  local text=$2
  local timeout=${3:-10}
  local attempts=$((timeout * 40))
  local i
  for ((i = 0; i < attempts; i++)); do
    if [[ -f "$path" ]] && grep -Fq -- "$text" "$path"; then
      return 0
    fi
    sleep 0.025
  done
  fail "timed out waiting for '$text' in $path"
}

send_json() {
  local base=$1
  local kind=$2
  local message=$3
  SAFE_RUNTIME_CONFIG="$base/safe.yaml" \
    "$SAFECTL_BIN" send "$kind" --json "$message" >/dev/null
}

assert_journal_bounds() {
  local path=$1
  local max_bytes=$2
  local max_records=$3
  local bytes=0
  local records=0
  if [[ -f "$path" ]]; then
    bytes=$(wc -c <"$path")
    records=$(jq -s 'length' "$path")
  fi
  ((bytes <= max_bytes)) || fail "$path is $bytes bytes, limit is $max_bytes"
  ((records <= max_records)) || fail "$path has $records records, limit is $max_records"
}

wait_for_journal_bounds() {
  local path=$1
  local max_bytes=$2
  local max_records=$3
  local timeout=${4:-10}
  local attempts=$((timeout * 40))
  local stable=0
  local bytes records i
  for ((i = 0; i < attempts; i++)); do
    bytes=0
    records=0
    if [[ -f "$path" ]]; then
      bytes=$(wc -c <"$path")
      records=$(jq -s 'length' "$path" 2>/dev/null || printf '%s' $((max_records + 1)))
    fi
    if ((bytes <= max_bytes && records <= max_records)); then
      stable=$((stable + 1))
      ((stable >= 5)) && return 0
    else
      stable=0
    fi
    sleep 0.025
  done
  fail "$path did not settle within $max_bytes bytes and $max_records records"
}

wait_for_attempts() {
  local path=$1
  local minimum=$2
  local timeout=${3:-10}
  local attempts=$((timeout * 40))
  local value=0
  local i
  for ((i = 0; i < attempts; i++)); do
    if [[ -f "$path" ]]; then
      value=$(<"$path")
    else
      value=0
    fi
    if [[ "$value" =~ ^[0-9]+$ ]] && ((value >= minimum)); then
      return 0
    fi
    sleep 0.025
  done
  fail "timed out waiting for $minimum attempts in $path"
}

scenario_bounded_replay() {
  local base=$TMP_ROOT/bounded-replay
  local state=$base/state
  local log=$base/safe-process.log
  local command_a='1:00000000-0000-0000-0000-000000000001:0'
  local command_b='2:00000000-0000-0000-0000-000000000001:0'
  mkdir -p "$state"
  printf '[]\n' >"$base/modes.json"

  cat >"$state/outputs.jsonl" <<EOF
{"Board":{"Proposed":{"id":"$command_a","from":"00000000-0000-0000-0000-000000000001","cmd":{"Scheduled":{"cmd":"PointNadir","gps_time":70.0}},"ts_mono":1}}}
{"Board":{"Approved":{"id":"$command_a","by":"00000000-0000-0000-0000-000000000000","reason":"approved before restart","ts_mono":2}}}
{"Board":{"Proposed":{"id":"$command_b","from":"00000000-0000-0000-0000-000000000001","cmd":{"Scheduled":{"cmd":"PointSunYaw","gps_time":80.0}},"ts_mono":3}}}
{"Board":{"Approved":{"id":"$command_b","by":"00000000-0000-0000-0000-000000000000","reason":"approved before rejection","ts_mono":4}}}
{"Board":{"Canceled":{"id":"$command_b","by":"00000000-0000-0000-0000-000000000001","reason":"injected cancellation","ts_mono":5}}}
EOF

  cat >"$base/safe.yaml" <<EOF
base_paths:
  base_working_directory: "$base"
  base_writable_directory: "$base"
logging:
  file_path: "$base/logs/safe.log"
persistence:
  events_max_bytes: 4096
  events_max_records: 2
  outputs_max_bytes: 262144
  outputs_max_records: 3
platform:
  telemetry_adapter: example
  command_adapter: safectl_unix_json
  egress_adapter: safectl_filesystem
  gatekeeper_adapter: disabled
EOF

  start_safe "$base" "$log"
  local pid=$LAST_PID
  wait_for_path "$state/safectl.sock"
  wait_for_json "$state/status.json" ".board[]? | select(.id == \"$command_a\" and .state == \"published\")"
  wait_for_json "$state/status.json" ".board[]? | select(.id == \"$command_b\" and .state == \"rejected\")"

  local i
  for i in 1 2 3 4 5 6; do
    send_json "$base" telemetry "{\"type\":\"telemetry\",\"telemetry\":{\"source\":\"edge-$i\",\"ts_mono\":$i,\"payload\":\"{\\\"counter\\\":$i}\"}}"
  done
  send_json "$base" command '{"type":"command","command":{"kind":"execute_now","command":"PointNadir"},"request_id":"edge-command-1"}'
  send_json "$base" command '{"type":"command","command":{"kind":"execute_now","command":"PointSunYaw"},"request_id":"edge-command-2"}'

  wait_for_text "$state/host_command_status.jsonl" 'edge-command-2'
  wait_for_json "$state/flight.json" '.last_seq_applied >= 8'
  wait_for_json "$state/outputs.jsonl" 'has("Snapshot")'
  wait_for_journal_bounds "$state/outputs.jsonl" 262144 3

  stop_safe "$pid"
  local crash_event_seq
  crash_event_seq=$(jq -s '[.[].seq] | max // 0' "$state/events.jsonl")
  local event_bytes
  event_bytes=$(wc -c <"$state/events.jsonl")
  ((event_bytes <= 4096)) || fail "event journal exceeded its hard byte limit"
  assert_journal_bounds "$state/outputs.jsonl" 262144 3

  rm -f -- "$state/status.json" "$state/safectl.sock"
  start_safe "$base" "$log.restart"
  pid=$LAST_PID
  wait_for_json "$state/flight.json" ".last_seq_applied >= $crash_event_seq"
  wait_for_json "$state/status.json" ".board[]? | select(.id == \"$command_a\" and .state == \"published\")"
  wait_for_json "$state/status.json" ".board[]? | select(.id == \"$command_b\" and .state == \"rejected\" and .decision_reason == \"injected cancellation\")"
  wait_for_journal_bounds "$state/events.jsonl" 4096 2
  wait_for_journal_bounds "$state/outputs.jsonl" 262144 3
  kill -0 "$pid" 2>/dev/null || fail "SAFE exited after bounded-journal restart"
  stop_safe "$pid"
  pass "bounded journals compact and resolved board state survives restart"
}

scenario_external_egress_recovery() {
  local base=$TMP_ROOT/external-egress
  local state=$base/state
  local log=$base/safe-process.log
  local command_id='10:00000000-0000-0000-0000-000000000001:0'
  local attempts=$base/egress-attempts
  local child_pid_file=$base/egress.pid
  local egress_script=$base/egress.sh
  mkdir -p "$state"
  printf '[]\n' >"$base/modes.json"

  {
    printf '{"Board":{"Proposed":{"id":"%s","from":"00000000-0000-0000-0000-000000000001","cmd":{"Scheduled":{"cmd":"PointNadir","gps_time":90.0}},"ts_mono":10}}}\n' "$command_id"
    printf '{"Board":{"Approved":{"id":"%s","by":"00000000-0000-0000-0000-000000000000","reason":"egress edge test","ts_mono":11}}}\n' "$command_id"
    printf '{"Board":{"Canceled":{"id":"padding","by":"00000000-0000-0000-0000-000000000001","reason":"'
    dd if=/dev/zero bs=1024 count=128 2>/dev/null | tr '\0' x
    printf '","ts_mono":12}}}\n'
  } >"$state/outputs.jsonl"

  cat >"$egress_script" <<EOF
#!/usr/bin/env bash
set -eu
attempt=0
[[ -f "$attempts" ]] && attempt=\$(cat "$attempts")
attempt=\$((attempt + 1))
printf '%s' "\$attempt" >"$attempts"
printf '%s' "\$\$" >"$child_pid_file"
case "\$attempt" in
  1) exit 42 ;;
  2) exec sleep 5 ;;
esac
while IFS= read -r line; do
  case "\$line" in
    *board_snapshot*)
      printf '%s\n' '{"kind":"board_published","command_ids":["$command_id"]}'
      ;;
  esac
done
EOF
  chmod +x "$egress_script"

  cat >"$base/safe.yaml" <<EOF
base_paths:
  base_working_directory: "$base"
  base_writable_directory: "$base"
logging:
  file_path: "$base/logs/safe.log"
persistence:
  events_max_bytes: 65536
  events_max_records: 10
  outputs_max_bytes: 524288
  outputs_max_records: 10
platform:
  telemetry_adapter: example
  command_adapter: safectl_unix_json
  egress_adapter: external
  external_egress_command: "exec bash $egress_script"
  external_egress_retry:
    initial_delay_ms: 50
    max_delay_ms: 100
    stable_session_ms: 500
    write_timeout_ms: 200
  gatekeeper_adapter: disabled
EOF

  start_safe "$base" "$log"
  local pid=$LAST_PID
  wait_for_attempts "$attempts" 3 10
  wait_for_json "$state/status.json" ".board[]? | select(.id == \"$command_id\" and .state == \"published\")" 10
  wait_for_text "$log" 'external platform egress adapter failed; restarting' 10
  wait_for_text "$log" 'timed out writing to external egress process' 10

  local child_pid
  child_pid=$(cat "$child_pid_file")
  kill "$child_pid"
  wait_for_attempts "$attempts" 4 10
  kill -0 "$pid" 2>/dev/null || fail "SAFE exited while restarting external egress"
  stop_safe "$pid"
  pass "external egress recovers from exit, write stall, and later process death"
}

printf 'Runtime edge workspace: %s\n' "$TMP_ROOT"
scenario_bounded_replay
scenario_external_egress_recovery
printf 'All runtime edge scenarios passed.\n'
