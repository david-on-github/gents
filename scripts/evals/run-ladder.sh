#!/usr/bin/env bash
# Run one configurator ladder level (or all six) with `gents eval run` against
# the checkout this script lives in.
#
#   scripts/evals/run-ladder.sh LEVEL [TRIALS] [CONCURRENCY]
#
# LEVEL is l1..l6, a full id (l4-automation) or `all` (levels in order).
# TRIALS defaults to 3. CONCURRENCY (default 4) caps in-flight inference calls:
# trials run at CONCURRENCY / GENTS_EVAL_PER_TRIAL at once, and each trial's
# backend admits GENTS_EVAL_PER_TRIAL (default 1) calls.
#
# Environment:
#   GENTS_EVAL_TARGET   scripts/evals/targets/<name>.json (default workstation-1)
#   GENTS_EVAL_SPLITS   splits to run, in order (default "train validation"; "none" only sets up)
#   GENTS_EVAL_HOME     eval home (default ~/gents-eval-homes/ladder-<short sha>)
#   GENTS_EVAL_PORT     port the eval home is served on (default 9493)
#   GENTS_EVAL_PER_TRIAL  inference calls one trial may have in flight (default 1)
#   GENTS_EVAL_REASONING / GENTS_EVAL_TEMPERATURE / GENTS_EVAL_TOP_P
#                       Engineer sampling (default high / 1.0 / 0.95)
#   GENTS_BIN           use this gents binary instead of building one
set -euo pipefail

usage() { sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
[ $# -ge 1 ] || usage

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
LADDER="$ROOT/crates/gents/tests/fixtures/configurator_evals/ladder"
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

LEVELS=(l1-inference l2-agent l3-datastore l4-automation l5-agents-tools l6-graph)
case "$1" in
  all) SELECTED=("${LEVELS[@]}") ;;
  l[1-6]) SELECTED=("${LEVELS[$(( ${1#l} - 1 ))]}") ;;
  *) SELECTED=("$1") ;;
esac
for level in "${SELECTED[@]}"; do
  [ -d "$LADDER/${level//-/_}" ] || { echo "unknown level $level (known: ${LEVELS[*]})" >&2; exit 2; }
done
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

target_field() { python3 -c 'import json,sys; t=json.load(open(sys.argv[1])); print(eval(sys.argv[2]))' "$TARGET_FILE" "$1"; }
ENDPOINT=$(target_field 't["inference_backends"][0]["endpoint"]')
MODEL=$(target_field 't["inference_profiles"][0]["model_name"]')

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

STAMP=$(date +%Y%m%d-%H%M)
for level in "${SELECTED[@]}"; do
  # A definition pack's empty agent_principal would clear the home's default
  # behavior, and the served home then refuses to restart; keep the default.
  DEFINITION="$EVAL_HOME/definitions/$level"
  rm -rf "$DEFINITION" && mkdir -p "$EVAL_HOME/definitions" && cp -R "$LADDER/${level//-/_}" "$DEFINITION"
  python3 -c 'import json,sys; p=sys.argv[1]; c=json.load(open(p)); c["agent_principal"]={"default_behavior_id": sys.argv[2]}; json.dump(c, open(p,"w"), indent=2)' \
    "$DEFINITION/pack_config.json" "$DID:default"
  "$GENTS" config apply --root "$DEFINITION" --bind-agent-did home --home "$EVAL_HOME" >/dev/null
  for split in $SPLITS; do
    [ "$split" != none ] || continue
    RUN_ID="ladder-$level-$SHA-$split-$STAMP"
    cat >&2 <<EOF

== $level ($split): $TRIALS trials per case, $TRIAL_CONCURRENCY trials at once, $PER_TRIAL call(s) per trial, $MODEL at $ENDPOINT
watch:  $GENTS eval watch $RUN_ID --home $EVAL_HOME
EOF
    status=0
    (cd "$EVAL_HOME/work" && "$GENTS" eval run "configurator-$level" \
      --cell "engineer=$SUBJECT:engineer" --profile "engineer=$PROFILE_ID" \
      --split "$split" --trials "$TRIALS" --concurrency "$TRIAL_CONCURRENCY" \
      --run-id "$RUN_ID" --home "$EVAL_HOME" 2>>"$EVAL_HOME/eval-run.log") || status=$?
    cat >&2 <<EOF
report: $GENTS eval show $RUN_ID --home $EVAL_HOME
trial:  $GENTS eval trial $RUN_ID engineer <case_id> [index] --home $EVAL_HOME
EOF
    [ "$status" = 0 ] || echo "eval run exited $status; resume with: $GENTS eval resume $RUN_ID --home $EVAL_HOME" >&2
  done
done
echo "the eval home stays served on $PORT; stop it with: kill \$(cat $EVAL_HOME/server.pid)" >&2
