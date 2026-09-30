#!/usr/bin/env bash
# Run native onboarding evals with a live terminal view.
#
#   scripts/evals/run-ladder.sh [all|l1..l6|factory-setup|list] [TRIALS] [CONCURRENCY]
#
# `all` runs L1-L5 and the configuration-only capstone. L6 is an explicit
# capability probe, excluded from acceptance while graph authoring is missing.
# Defaults: 3 trials per case; 4 concurrent inference calls; train + validation.
# Held-out cases run only with GENTS_EVAL_SPLITS=held_out.
#
# GENTS_EVAL_TARGET: target file name (default workstation-1).
# GENTS_EVAL_HOME / GENTS_EVAL_PORT: isolated eval home / port (default 9493).
# GENTS_EVAL_PER_TRIAL: concurrent calls per trial (default 1).
# GENTS_EVAL_REASONING / GENTS_EVAL_TEMPERATURE / GENTS_EVAL_TOP_P: high / 1 / .95.
# GENTS_EVAL_WATCH: auto, 1 or 0. Auto uses the native TUI in a terminal.
# GENTS_BIN: existing gents binary; otherwise builds this checkout.
set -euo pipefail

usage() { sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
[ $# -le 3 ] || usage
SELECTION=${1:-all}

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
FIXTURES="$ROOT/crates/gents/tests/fixtures/configurator_evals"
LADDER="$FIXTURES/ladder"
MATRIX="$ROOT/scripts/evals/matrix.json"
SHA=$(git -C "$ROOT" rev-parse --short HEAD)
TRIALS=${2:-3}
CONCURRENCY=${3:-4}
PER_TRIAL=${GENTS_EVAL_PER_TRIAL:-1}
TARGET=${GENTS_EVAL_TARGET:-workstation-1}
SPLITS=${GENTS_EVAL_SPLITS:-train validation}
EVAL_HOME=${GENTS_EVAL_HOME:-$HOME/gents-eval-homes/ladder-$SHA}
PORT=${GENTS_EVAL_PORT:-9493}
REASONING=${GENTS_EVAL_REASONING:-high}
TEMPERATURE=${GENTS_EVAL_TEMPERATURE:-1.0}
TOP_P=${GENTS_EVAL_TOP_P:-0.95}
TARGET_FILE="$ROOT/scripts/evals/targets/$TARGET.json"

if [ "$SELECTION" = list ]; then
  python3 - "$MATRIX" "$FIXTURES" "$TRIALS" <<'PYLIST'
import collections, json, pathlib, sys
matrix=json.load(open(sys.argv[1])); root=pathlib.Path(sys.argv[2]); trials=int(sys.argv[3])
print(f"{'Suite':<20} {'Train':>5} {'Valid':>5} {'Held':>5} {'Stages':>6} {'Checks':>6}  Coverage")
for suite in matrix['suites']:
    definition=json.load(open(root/suite['path']/'pack_config.json'))['eval_definitions'][0]
    cases=[json.load(open(root/suite['path']/p)) for p in definition['cases']]
    splits=collections.Counter(c['split'] for c in cases)
    stages=[s for c in cases for s in c['stages']]
    print(f"{suite['id']:<20} {splits['train']:>5} {splits['validation']:>5} {splits['held_out']:>5} {len(stages):>6} {sum(len(s['checks']) for s in stages):>6}  {', '.join(suite['coverage'])}")
    if 'blocked_reason' in suite: print('  '+suite['blocked_reason'])
print(f"Stage/check totals include held-out cases; each selected case runs {trials} trials. Capstone stops at configuration.")
PYLIST
  exit 0
fi
SELECTED=()
while IFS= read -r level; do SELECTED+=("$level"); done < <(python3 - "$MATRIX" "$SELECTION" <<'PYSELECT'
import json,sys
m=json.load(open(sys.argv[1])); selection=sys.argv[2]
if selection=='all': print('\n'.join(m['default']))
else:
    matches=[s['id'] for s in m['suites'] if selection in [s['id'],s['id'].split('-')[0]]]
    if len(matches)==1: print(matches[0])
PYSELECT
)
[ ${#SELECTED[@]} -gt 0 ] || { echo "unknown suite $SELECTION; use list" >&2; exit 2; }
[[ "$TRIALS" =~ ^[1-9][0-9]*$ && "$CONCURRENCY" =~ ^[1-9][0-9]*$ && "$PER_TRIAL" =~ ^[1-9][0-9]*$ ]] || usage
[ -f "$TARGET_FILE" ] || { echo "no target $TARGET_FILE" >&2; exit 2; }
[ "$PORT" != 9191 ] || { echo "port 9191 belongs to the desktop node; pick another GENTS_EVAL_PORT" >&2; exit 2; }
(( PER_TRIAL >= 1 && CONCURRENCY >= PER_TRIAL )) || { echo "CONCURRENCY must be at least GENTS_EVAL_PER_TRIAL" >&2; exit 2; }
TRIAL_CONCURRENCY=$(( CONCURRENCY / PER_TRIAL ))

if [ -n "${GENTS_BIN:-}" ]; then
  GENTS=$GENTS_BIN
else
  echo "building gents at $SHA ..." >&2
  cargo build --quiet --manifest-path "$ROOT/Cargo.toml" -p gents-cli --bin gents
  GENTS="$ROOT/target/debug/gents"
fi

target_field() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]][0][sys.argv[3]])' "$TARGET_FILE" "$1" "$2"; }
ENDPOINT=$(target_field inference_backends endpoint)
MODEL=$(target_field inference_profiles model_name)

mkdir -p "$EVAL_HOME/work"
if [ ! -f "$EVAL_HOME/init.json" ]; then
  echo "initializing $EVAL_HOME ..." >&2
  (cd "$EVAL_HOME/work" && "$GENTS" init --home "$EVAL_HOME" --write --inference-url "$ENDPOINT" \
    --model-name "$MODEL" --max-concurrent "$PER_TRIAL" --tool-root "$EVAL_HOME/work" >/dev/null)
fi
DID=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["agent_did"])' "$EVAL_HOME/init.json")

GRAPHQL="http://127.0.0.1:$PORT/api/v0/graphql"
# Served means the endpoint answers and runtime.json names it: CLI commands
# with --home fall back to opening the store themselves until runtime.json does.
served() {
  grep -qF "127.0.0.1:$PORT/" "$EVAL_HOME/runtime.json" 2>/dev/null &&
    curl -fsS -m 3 -H 'content-type: application/json' -d '{"query":"{ AgentPrincipal { agent_did } }"}' "$GRAPHQL" >/dev/null 2>&1
}
if ! served; then
  echo "serving $EVAL_HOME on $PORT (log $EVAL_HOME/server.log) ..." >&2
  (cd "$EVAL_HOME/work" && exec nohup "$GENTS" server --home "$EVAL_HOME" --http-port "$PORT" \
    </dev/null >"$EVAL_HOME/server.log" 2>&1) &
  echo $! >"$EVAL_HOME/server.pid"
  disown
  for _ in $(seq 1 90); do served && break; sleep 2; done
  served || { echo "the eval home did not come up; see $EVAL_HOME/server.log" >&2; exit 1; }
fi

# The trial copies the backend, sampling, profile and the profile's bound
# InferenceExecution the run freezes; the ladder profile reuses the execution
# `gents init` created for the home's default profile.
PROFILE_ID="$DID:ladder-$TARGET"
INFERENCE="$EVAL_HOME/inference-$TARGET"
python3 - "$TARGET_FILE" "$DID" "$PER_TRIAL" "$INFERENCE" "$REASONING" "$TEMPERATURE" "$TOP_P" <<'PY'
import json, os, sys
target, did, per_trial, root, reasoning, temperature, top_p = sys.argv[1:8]
t = json.load(open(target))
name = os.path.basename(target)[:-5]
backend = dict(t["inference_backends"][0])
backend.update(backend_id=f"{did}:ladder-{name}", agent_did=did, max_concurrent=int(per_trial))
sampling = {"sampling_id": f"{did}:ladder-{name}-sampling", "agent_did": did,
            "display_name": f"Ladder {name}", "temperature": float(temperature), "top_p": float(top_p)}
profile = {"profile_id": f"{did}:ladder-{name}", "agent_did": did, "display_name": f"Ladder {name}",
           "backend_id": backend["backend_id"], "model_name": t["inference_profiles"][0]["model_name"],
           "reasoning_effort": reasoning, "sampling_id": sampling["sampling_id"],
           "execution_id": f"{did}:default-profile-execution"}
os.makedirs(root, exist_ok=True)
json.dump({"manifest_version": 1, "name": "ladder_inference", "version": "0.1.0",
           "description": "The ladder's trial inference binding", "authors": ["gents-ai contributors"],
           "tags": ["eval"], "kind": "documents", "assets": ["pack_config.json"], "config": "pack_config.json"},
          open(f"{root}/manifest.json", "w"))
json.dump({"agent_principal": {"default_behavior_id": f"{did}:default"}, "inference_backends": [backend], "inference_sampling": [sampling],
           "inference_profiles": [profile]}, open(f"{root}/pack_config.json", "w"), indent=2)
PY
"$GENTS" config apply --root "$INFERENCE" --bind-agent-did home --home "$EVAL_HOME" >/dev/null
echo "profile $PROFILE_ID: $MODEL, reasoning $REASONING, temperature $TEMPERATURE, top_p $TOP_P, $PER_TRIAL call(s) per trial" >&2

# The subject is the Engineer this checkout seeds: its Setup prompt and grant,
# copied over the pack's so the cell never drifts from gents_protocol.
SUBJECT="$EVAL_HOME/engineer_subject-$SHA"
rm -rf "$SUBJECT" && cp -R "$LADDER/engineer_subject" "$SUBJECT"
cp "$ROOT/crates/gents-protocol/prompts/setup.md" "$SUBJECT/agent_behaviors/engineer/system_prompt.md"
python3 - "$SUBJECT/pack_config.json" "$ROOT/crates/gents-protocol/presets/setup-self-config.json" <<'PY'
import json, sys
config, grant = sys.argv[1], sys.argv[2]
c = json.load(open(config))
c["tools"][0]["self_config"] = json.load(open(grant))
json.dump(c, open(config, "w"), indent=2)
PY

STAMP=$(date +%Y%m%d-%H%M%S)
MATRIX_STATUS=0
WATCH=${GENTS_EVAL_WATCH:-auto}
if [ "$WATCH" = auto ]; then WATCH=0; [ ! -t 1 ] || WATCH=1; fi
for level in "${SELECTED[@]}"; do
  SUITE_PATH=$(python3 -c 'import json,sys; print(next(s["path"] for s in json.load(open(sys.argv[1]))["suites"] if s["id"]==sys.argv[2]))' "$MATRIX" "$level")
  DEFINITION_ID=$(python3 -c 'import json,sys; print(next(s["definition_id"] for s in json.load(open(sys.argv[1]))["suites"] if s["id"]==sys.argv[2]))' "$MATRIX" "$level")
  RUN_SUBJECT=$SUBJECT
  if [ "$level" = factory-setup ]; then
    RUN_SUBJECT="$EVAL_HOME/factory_subject-$SHA"
    rm -rf "$RUN_SUBJECT"
    cp -R "$FIXTURES/factory_setup/engineer_kspec" "$RUN_SUBJECT"
    cp "$ROOT/crates/gents-protocol/prompts/setup.md" "$RUN_SUBJECT/engineer/system_prompt.md"
  fi
  # A definition pack's empty agent_principal would clear the home's default
  # behavior, and the served home then refuses to restart; keep the default.
  DEFINITION="$EVAL_HOME/definitions/$level"
  rm -rf "$DEFINITION" && mkdir -p "$EVAL_HOME/definitions" && cp -R "$FIXTURES/$SUITE_PATH" "$DEFINITION"
  python3 -c 'import json,sys; p=sys.argv[1]; c=json.load(open(p)); c["agent_principal"]={"default_behavior_id": sys.argv[2]}; json.dump(c, open(p,"w"), indent=2)' \
    "$DEFINITION/pack_config.json" "$DID:default"
  "$GENTS" config apply --root "$DEFINITION" --bind-agent-did home --home "$EVAL_HOME" >/dev/null
  for split in $SPLITS; do
    [ "$split" != none ] || continue
    if ! python3 - "$DEFINITION" "$split" <<'PYSPLIT'
import json,pathlib,sys
root=pathlib.Path(sys.argv[1]); definition=json.load(open(root/'pack_config.json'))['eval_definitions'][0]
sys.exit(0 if any(json.load(open(root/p))['split']==sys.argv[2] for p in definition['cases']) else 1)
PYSPLIT
    then continue; fi
    RUN_ID="ladder-$level-$SHA-$split-$STAMP"
    cat >&2 <<EOF

== $level ($split): $TRIALS trials per case, $TRIAL_CONCURRENCY trials at once, $PER_TRIAL call(s) per trial, $MODEL at $ENDPOINT
watch:  $GENTS eval watch $RUN_ID --home $EVAL_HOME
EOF
    status=0
    RUN_LOG="$EVAL_HOME/$RUN_ID.log"
    (cd "$EVAL_HOME/work" && exec "$GENTS" eval run "$DEFINITION_ID" \
      --cell "engineer=$RUN_SUBJECT:engineer" --profile "engineer=$PROFILE_ID" \
      --split "$split" --trials "$TRIALS" --concurrency "$TRIAL_CONCURRENCY" \
      --run-id "$RUN_ID" --home "$EVAL_HOME") >"$RUN_LOG" 2>&1 &
    RUN_PID=$!
    trap 'echo "Eval continues in process $RUN_PID. Watch: $GENTS eval watch $RUN_ID --home $EVAL_HOME" >&2; disown "$RUN_PID" 2>/dev/null || true; exit 130' INT
    if [ "$WATCH" = 1 ]; then
      while kill -0 "$RUN_PID" 2>/dev/null; do
        if [ -f "$EVAL_HOME/eval/runs/$RUN_ID/progress.json" ] || [ -f "$EVAL_HOME/eval/runs/$RUN_ID/report.json" ]; then
          "$GENTS" eval watch "$RUN_ID" --home "$EVAL_HOME" || true
          break
        fi
        sleep 1
      done
    else
      echo "run log: $RUN_LOG" >&2
    fi
    wait "$RUN_PID" || status=$?
    trap - INT
    if [ "$status" != 0 ]; then MATRIX_STATUS=$status; tail -n 20 "$RUN_LOG" >&2; fi
    cat >&2 <<EOF
report: $GENTS eval show $RUN_ID --home $EVAL_HOME
trial:  $GENTS eval trial $RUN_ID engineer <case_id> [index] --home $EVAL_HOME
EOF
    [ "$status" = 0 ] || echo "eval run exited $status; resume with: $GENTS eval resume $RUN_ID --home $EVAL_HOME" >&2
  done
done
echo "the eval home stays served on $PORT; stop it with: kill \$(cat $EVAL_HOME/server.pid)" >&2

exit "$MATRIX_STATUS"
