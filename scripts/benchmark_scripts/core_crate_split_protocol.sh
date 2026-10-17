#!/usr/bin/env bash
# Cargo --timings protocols for the codex-core crate-split work.
#
# usage:
#   core_crate_split_protocol.sh cold LABEL ROOT OUT
#   core_crate_split_protocol.sh sequential LABEL ROOT VALIDATION_FILE OWN_TEST_PACKAGE OUT [EXTRA_CLEAN_PACKAGE]
#   core_crate_split_protocol.sh paired BEFORE_ROOT AFTER_ROOT OUT [EXTRA_CLEAN_PACKAGE]
#
# Each ROOT is a snapshot holding codex-rs/ and its own target/; OUT collects
# the reports. VALIDATION_FILE is validation.rs relative to codex-rs, and
# OWN_TEST_PACKAGE is the package whose lib tests cover it. EXTRA_CLEAN_PACKAGE
# is the extracted crate, cleaned with codex-core for from-scratch builds.
#   cold:       0a build -p codex-core into an empty target; 0b lib-test build.
#   sequential: 1a/1b core from scratch; 2 edit turn.rs, build; w lib-test
#               build; 3 edit turn.rs, lib-test build; 4 edit validation.rs,
#               build; 5 edit validation.rs, build OWN_TEST_PACKAGE's lib tests.
#   paired:     before and after trees build at the same time, so both see the
#               same outside load: a warm-up, then three reps each of 3 and 2,
#               then three reps each of 1 and 4 (core from scratch, lib-test
#               build).
# Summarize with core_crate_split_timings.py `rounds OUT` or `paired OUT`.
# Run one mode at a time. Probe edits are reverted on exit.
# Never edit this file while a run is executing: bash reads scripts incrementally.
set -euo pipefail
T="python -I -B $(dirname "$0")/core_crate_split_timings.py"
TURN_FILE=core/src/session/turn.rs
TURN_ORIG='warn!("plugin mention telemetry enrichment capacity exhausted");'
VAL_ORIG='"execution_context": self.execution_context,'

usage() {
  sed -n '4,7p' "$0" >&2
  exit 2
}

turn_text() {
  if [ -z "$1" ]; then
    echo "$TURN_ORIG"
  else
    echo "warn!(\"plugin mention telemetry enrichment capacity exhausted ($1)\");"
  fi
}

declare -A PROBE_ORIG PROBE_CUR
probe() { # FILE ORIGINAL NEW
  local file=$1 orig=$2 new=$3
  local cur=${PROBE_CUR[$file]:-$orig}
  if [ "$cur" = "$new" ]; then
    return 0
  fi
  $T probe --file "$file" --find "$cur" --replace "$new" >/dev/null
  PROBE_ORIG[$file]=$orig
  PROBE_CUR[$file]=$new
}
revert_probes() {
  local file
  for file in "${!PROBE_CUR[@]}"; do
    if [ "${PROBE_CUR[$file]}" != "${PROBE_ORIG[$file]}" ]; then
      $T probe --file "$file" --find "${PROBE_CUR[$file]}" --replace "${PROBE_ORIG[$file]}" >/dev/null \
        || echo "could not revert the probe in $file" >&2
    fi
  done
}
trap revert_probes EXIT

run_one() { # ROOT OUT LABEL [--clean PKG ...] -- CARGO-ARGS...
  local root=$1 out=$2 label=$3
  shift 3
  $T run --codex-rs "$root/codex-rs" --target-dir "$root/target" --out "$out" --label "$label" \
    --unit codex-core --unit core_test_support "$@"
}

cold() { # LABEL ROOT OUT
  local label=$1 root=$2 out=$3
  if [ -d "$root/target" ] && [ -n "$(ls -A "$root/target")" ]; then
    echo "cold needs an empty $root/target" >&2
    exit 2
  fi
  run_one "$root" "$out" "$label-0a-cold-build" -- build -p codex-core
  run_one "$root" "$out" "$label-0b-cold-libtest" -- test -p codex-core --lib --no-run
}

sequential() { # LABEL ROOT VALIDATION_FILE OWN_TEST_PACKAGE OUT [EXTRA_CLEAN_PACKAGE]
  local label=$1 root=$2 own=$4 out=$5 extra=${6:-}
  local turn=$root/codex-rs/$TURN_FILE validation=$root/codex-rs/$3
  local clean="--clean codex-core"
  if [ -n "$extra" ]; then
    clean="$clean --clean $extra"
  fi
  run_one "$root" "$out" "$label-1a-core-rebuild" $clean -- build -p codex-core
  run_one "$root" "$out" "$label-1b-core-rebuild" $clean -- build -p codex-core
  probe "$turn" "$TURN_ORIG" "$(turn_text 'timing probe a')"
  run_one "$root" "$out" "$label-2-edit-turn-build" -- build -p codex-core
  run_one "$root" "$out" "$label-w-libtest-warm" -- test -p codex-core --lib --no-run
  probe "$turn" "$TURN_ORIG" "$(turn_text 'timing probe b')"
  run_one "$root" "$out" "$label-3-edit-turn-libtest" -- test -p codex-core --lib --no-run
  probe "$validation" "$VAL_ORIG" '"execution_context_probe_a": self.execution_context,'
  run_one "$root" "$out" "$label-4-edit-validation-build" -- build -p codex-core
  probe "$turn" "$TURN_ORIG" "$TURN_ORIG"
  probe "$validation" "$VAL_ORIG" "$VAL_ORIG"
  run_one "$root" "$out" "$label-5-edit-validation-own-tests-warmup" -- test -p "$own" --lib --no-run
  probe "$validation" "$VAL_ORIG" '"execution_context_step5": self.execution_context,'
  run_one "$root" "$out" "$label-5-edit-validation-own-tests" -- test -p "$own" --lib --no-run
  probe "$validation" "$VAL_ORIG" "$VAL_ORIG"
}

pair() { # BEFORE_ROOT AFTER_ROOT OUT NAME "BEFORE-CLEAN" "AFTER-CLEAN" -- CARGO-ARGS...
  local before=$1 after=$2 out=$3 name=$4 bclean=$5 aclean=$6
  shift 6
  local rb=0 ra=0 pb pa
  run_one "$before" "$out" "before-$name" $bclean "$@" > "$out/before-$name.stdout" 2>&1 &
  pb=$!
  run_one "$after" "$out" "after-$name" $aclean "$@" > "$out/after-$name.stdout" 2>&1 &
  pa=$!
  wait $pb || rb=$?
  wait $pa || ra=$?
  head -1 "$out/before-$name.stdout"
  head -1 "$out/after-$name.stdout"
  if [ $rb != 0 ] || [ $ra != 0 ]; then
    echo "pair $name failed: before=$rb after=$ra" >&2
    exit 1
  fi
}

paired() { # BEFORE_ROOT AFTER_ROOT OUT [EXTRA_CLEAN_PACKAGE]
  local before=$1 after=$2 out=$3 extra=${4:-}
  local aclean="--clean codex-core" k
  if [ -n "$extra" ]; then
    aclean="$aclean --clean $extra"
  fi
  mkdir -p "$out"
  set_turns() {
    probe "$before/codex-rs/$TURN_FILE" "$TURN_ORIG" "$(turn_text "$1")"
    probe "$after/codex-rs/$TURN_FILE" "$TURN_ORIG" "$(turn_text "$1")"
  }
  set_turns "paired warm libtest"
  pair "$before" "$after" "$out" warm-libtest "" "" -- test -p codex-core --lib --no-run
  for k in 1 2 3; do
    set_turns "paired libtest $k"
    pair "$before" "$after" "$out" "3-edit-turn-libtest-r$k" "" "" -- test -p codex-core --lib --no-run
  done
  set_turns "paired warm build"
  pair "$before" "$after" "$out" warm-build "" "" -- build -p codex-core
  for k in 1 2 3; do
    set_turns "paired build $k"
    pair "$before" "$after" "$out" "2-edit-turn-build-r$k" "" "" -- build -p codex-core
  done
  set_turns ""
  for k in 1 2 3; do
    pair "$before" "$after" "$out" "1-scratch-build-r$k" "--clean codex-core" "$aclean" -- build -p codex-core
  done
  for k in 1 2 3; do
    pair "$before" "$after" "$out" "4-scratch-libtest-r$k" "--clean codex-core" "$aclean" -- test -p codex-core --lib --no-run
  done
}

mode=${1:-}
if [ $# -gt 0 ]; then
  shift
fi
case $mode in
  cold) [ $# = 3 ] || usage; cold "$@" ;;
  sequential) { [ $# -ge 5 ] && [ $# -le 6 ]; } || usage; sequential "$@" ;;
  paired) { [ $# -ge 3 ] && [ $# -le 4 ]; } || usage; paired "$@" ;;
  *) usage ;;
esac
echo "$mode protocol complete"
