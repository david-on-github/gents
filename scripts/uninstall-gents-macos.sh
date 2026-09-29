#!/usr/bin/env bash
#
# uninstall-gents-macos.sh
#
# Removes all local resources that Gents creates on macOS:
#   - the ~/.gents agent home (data, keys, plugins, codex-ui, init/runtime state, p2p key)
#   - the macOS LaunchAgent background service (ai.gents.runtime)
#   - the macOS keychain identity item (com.source-inc.gents.identity)
#   - the CLI binary (/usr/local/bin/gents)
#   - the desktop app (/Applications/Gents.app)
#   - every Gents entry under ~/Library (support, caches, logs, WebKit,
#     preferences, saved state, containers, ...)
#
# It does NOT touch per-repo worktrees created under <repo>/.gents/workspaces/,
# since those live inside your project checkouts. Those are only reported.
#
# Usage:
#   ./uninstall-gents-macos.sh           # interactive (asks for confirmation)
#   ./uninstall-gents-macos.sh --yes     # no prompt
#   ./uninstall-gents-macos.sh --dry-run # show what would be removed, change nothing

set -euo pipefail

# ----------------------------------------------------------------------------
# Constants (mirrors the source of truth in the Gents codebase)
# ----------------------------------------------------------------------------
GENTS_HOME_DEFAULT="${GENTS_HOME:-$HOME/.gents}"
SERVICE_LABEL="ai.gents.runtime"
LAUNCH_AGENT_PLIST="$HOME/Library/LaunchAgents/${SERVICE_LABEL}.plist"
KEYCHAIN_SERVICE="com.source-inc.gents.identity"
BUNDLE_ID="com.source-inc.gents"
APP_DIR_NAME="gents"
APP_BUNDLE="/Applications/Gents.app"
CLI_BINARY="/usr/local/bin/gents"

# ~/Library subdirectories swept for Gents entries (step 6).
LIBRARY_SUBDIRS=(
  "Application Support"
  "Application Scripts"
  "Caches"
  "Containers"
  "Cookies"
  "Group Containers"
  "HTTPStorages"
  "LaunchAgents"
  "Logs"
  "Preferences"
  "Preferences/ByHost"
  "Saved Application State"
  "WebKit"
)
# Exact, case-sensitive name patterns identifying Gents entries.
LIBRARY_NAME_PATTERNS=(
  "$APP_DIR_NAME"                 # e.g. Application Support/gents
  "$APP_DIR_NAME.*"               # e.g. gents.log
  "$APP_DIR_NAME-*"               # e.g. gents-fixture-host
  "$BUNDLE_ID"                    # e.g. Caches/com.source-inc.gents
  "$BUNDLE_ID.*"                  # e.g. com.source-inc.gents.plist / .savedState
  "$BUNDLE_ID-*"                  # e.g. com.source-inc.gents-fixture-host
  "*.$BUNDLE_ID"                  # e.g. Group Containers/<TEAMID>.com.source-inc.gents
  "*.$BUNDLE_ID.*"
  "ai.gents.*"                    # e.g. LaunchAgents/ai.gents.runtime.plist
)

ASSUME_YES=0
DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --yes|-y) ASSUME_YES=1 ;;
    --dry-run|-n) DRY_RUN=1 ;;
    -h|--help)
      grep '^#' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "Unknown argument: $arg" >&2; exit 2 ;;
  esac
done

log()  { printf '  %s\n' "$*"; }
info() { printf '\n==> %s\n' "$*"; }

run() {
  # run <description> <command...>
  local desc="$1"; shift
  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "[dry-run] $desc"
    return 0
  fi
  log "$desc"
  "$@"
}

# rm -rf helper that only acts on existing paths and honours dry-run.
remove_path() {
  local path="$1"
  if [[ -e "$path" || -L "$path" ]]; then
    run "removing $path" rm -rf -- "$path"
  else
    log "not present: $path"
  fi
}

need_sudo_for() {
  # Returns 0 if the given path exists and is not writable by us.
  local path="$1"
  [[ -e "$path" ]] || return 1
  local parent
  parent="$(dirname "$path")"
  [[ ! -w "$path" || ! -w "$parent" ]]
}

sudo_remove_path() {
  local path="$1"
  if [[ ! -e "$path" && ! -L "$path" ]]; then
    log "not present: $path"
    return 0
  fi
  if need_sudo_for "$path"; then
    run "removing (sudo) $path" sudo rm -rf -- "$path"
  else
    run "removing $path" rm -rf -- "$path"
  fi
}

# ----------------------------------------------------------------------------
# Confirmation
# ----------------------------------------------------------------------------
cat <<EOF
This will remove Gents and its local data from this Mac:

  Agent home        : $GENTS_HOME_DEFAULT
  LaunchAgent       : $LAUNCH_AGENT_PLIST  (job: $SERVICE_LABEL)
  Keychain identity : service "$KEYCHAIN_SERVICE"
  CLI binary        : $CLI_BINARY
  Desktop app       : $APP_BUNDLE
  Library entries   : everything named $APP_DIR_NAME, $APP_DIR_NAME-*, $BUNDLE_ID*, or ai.gents.*
                      under ~/Library/{Application Support,Caches,Logs,WebKit,
                      HTTPStorages,Cookies,Preferences,Saved Application State,
                      Containers,Group Containers,Application Scripts,LaunchAgents}

Per-repo worktrees under <checkout>/.gents/workspaces/ are NOT removed.
EOF

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo
  echo "(dry-run mode: nothing will be changed)"
fi

if [[ "$ASSUME_YES" -ne 1 && "$DRY_RUN" -ne 1 ]]; then
  echo
  read -r -p "Proceed? [y/N] " reply
  case "$reply" in
    y|Y|yes|YES) ;;
    *) echo "Aborted."; exit 1 ;;
  esac
fi

# ----------------------------------------------------------------------------
# 1. Stop & remove the LaunchAgent background service
# ----------------------------------------------------------------------------
info "Stopping and removing the LaunchAgent service ($SERVICE_LABEL)"
uid="$(id -u)"
if [[ "$DRY_RUN" -eq 1 ]]; then
  log "[dry-run] launchctl bootout gui/$uid/$SERVICE_LABEL"
else
  # Best-effort: ignore errors if the job isn't loaded.
  launchctl bootout "gui/$uid/$SERVICE_LABEL" 2>/dev/null || true
  launchctl remove "$SERVICE_LABEL" 2>/dev/null || true
fi
remove_path "$LAUNCH_AGENT_PLIST"

# ----------------------------------------------------------------------------
# 2. Quit and remove the desktop app
# ----------------------------------------------------------------------------
info "Quitting and removing the desktop app"
if [[ "$DRY_RUN" -eq 1 ]]; then
  log "[dry-run] osascript quit \"Gents\""
else
  osascript -e 'tell application "Gents" to quit' 2>/dev/null || true
  # Fall back to a hard kill if it's still running.
  pkill -f "$APP_BUNDLE/Contents/MacOS/" 2>/dev/null || true
fi
sudo_remove_path "$APP_BUNDLE"

# ----------------------------------------------------------------------------
# 3. Remove the CLI binary
# ----------------------------------------------------------------------------
info "Removing the CLI binary"
sudo_remove_path "$CLI_BINARY"

# ----------------------------------------------------------------------------
# 4. Remove the agent home (~/.gents)
# ----------------------------------------------------------------------------
info "Removing the agent home"
# Safety guard: never rm -rf "/" or "$HOME" itself.
if [[ -z "$GENTS_HOME_DEFAULT" || "$GENTS_HOME_DEFAULT" == "/" || "$GENTS_HOME_DEFAULT" == "$HOME" ]]; then
  echo "Refusing to remove suspicious GENTS_HOME: '$GENTS_HOME_DEFAULT'" >&2
else
  remove_path "$GENTS_HOME_DEFAULT"
fi

# ----------------------------------------------------------------------------
# 5. Remove the macOS keychain identity
# ----------------------------------------------------------------------------
info "Removing the macOS keychain identity ($KEYCHAIN_SERVICE)"
if [[ "$DRY_RUN" -eq 1 ]]; then
  log "[dry-run] security delete-generic-password -s \"$KEYCHAIN_SERVICE\""
else
  # There may be one item per agent label; loop until none remain.
  while security find-generic-password -s "$KEYCHAIN_SERVICE" >/dev/null 2>&1; do
    if ! security delete-generic-password -s "$KEYCHAIN_SERVICE" >/dev/null 2>&1; then
      break
    fi
    log "deleted a keychain item for service $KEYCHAIN_SERVICE"
  done
  log "no remaining generic-password items for $KEYCHAIN_SERVICE"
  log "note: Secure Enclave keys (if that backend was used) must be removed"
  log "      from Keychain Access manually; they are not exportable via CLI."
fi

# ----------------------------------------------------------------------------
# 6. Remove every Gents entry under ~/Library
#    The app stores data under the plain "gents" name (e.g.
#    ~/Library/Application Support/gents); WebView, cache, log, and
#    preference entries are keyed by the bundle id. Rather than a fixed
#    list, sweep every standard Library location for entries whose names
#    match Gents exactly (case-sensitive, anchored), so unrelated items such
#    as ".../external_agents" are never matched.
# ----------------------------------------------------------------------------
info "Removing all Gents entries under ~/Library"
if [[ "$DRY_RUN" -ne 1 ]]; then
  # Drop cached preferences so cfprefsd doesn't rewrite the plist.
  defaults delete "$BUNDLE_ID" >/dev/null 2>&1 || true
fi
found_any=0
shopt -s nullglob
for subdir in "${LIBRARY_SUBDIRS[@]}"; do
  base="$HOME/Library/$subdir"
  [[ -d "$base" ]] || continue
  for pattern in "${LIBRARY_NAME_PATTERNS[@]}"; do
    for entry in "$base"/$pattern; do
      # Literal (non-wildcard) patterns survive nullglob; skip if absent.
      [[ -e "$entry" || -L "$entry" ]] || continue
      # Safety guard: only direct children of a Library subdirectory.
      [[ "$(dirname "$entry")" == "$base" ]] || continue
      found_any=1
      remove_path "$entry"
    done
  done
done
shopt -u nullglob
[[ "$found_any" -eq 1 ]] || log "no Gents entries found under ~/Library"

# ----------------------------------------------------------------------------
# 7. Report per-repo worktrees (informational only)
# ----------------------------------------------------------------------------
info "Per-repo worktrees (not removed)"
log "Gents may have created worktrees under <your-checkouts>/.gents/workspaces/."
log "Search for them with:"
log "  find \"\$HOME\" -type d -path '*/.gents/workspaces' 2>/dev/null"

info "Done."
if [[ "$DRY_RUN" -eq 1 ]]; then
  log "(dry-run: no changes were made)"
fi
