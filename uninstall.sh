#!/usr/bin/env bash
# Brain Engine — full uninstaller.
#
# Removes EVERYTHING the Brain Engine installs or generates:
#   • binaries          ~/.local/bin/brain, brain-daemon  (or $PREFIX/bin)
#   • Claude styles     ~/.claude/output-styles/brain-eco*.md
#   • global data       ~/.brain/  (config, providers, llm_state, cache, logs, memory, models)
#   • running daemon     stopped via its PID file before files are deleted
#   • per-project state  .brain/ dirs + .claude/hooks/{pre_prompt,post_response}.sh
#                        and the brain hook entries in .claude/settings.json
#   • the source repo    (only with --purge-source)
#
# Usage:
#   ./uninstall.sh                 # interactive, removes everything except the source repo
#   ./uninstall.sh --yes           # no confirmation prompt
#   ./uninstall.sh --dry-run       # show what would be removed, change nothing
#   ./uninstall.sh --purge-source  # ALSO delete this repository directory
#   PREFIX=/usr/local ./uninstall.sh   # match a custom install prefix
#
# Scope of project scan: $HOME (skipping common heavy dirs). Override with SCAN_ROOT=...

set -euo pipefail

# ── Colour helpers ────────────────────────────────────────────────────────────
if [ -t 1 ]; then
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; BOLD='\033[1m'; RESET='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; BOLD=''; RESET=''
fi
info() { printf "${BOLD}[brain]${RESET}  %s\n"      "$*"; }
ok()   { printf "${GREEN}[brain]${RESET}  %s\n"     "$*"; }
warn() { printf "${YELLOW}[brain]${RESET}  %s\n"    "$*" >&2; }
err()  { printf "${RED}[brain] error:${RESET}  %s\n" "$*" >&2; }

# ── Flags ─────────────────────────────────────────────────────────────────────
ASSUME_YES=0
DRY_RUN=0
PURGE_SOURCE=0
for arg in "$@"; do
    case "$arg" in
        --yes|-y)        ASSUME_YES=1 ;;
        --dry-run|-n)    DRY_RUN=1 ;;
        --purge-source)  PURGE_SOURCE=1 ;;
        -h|--help)
            sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) err "Unknown argument: $arg"; exit 2 ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
CLAUDE_STYLES_DIR="$HOME/.claude/output-styles"
GLOBAL_BRAIN_DIR="$HOME/.brain"
SCAN_ROOT="${SCAN_ROOT:-$HOME}"

# ── Action helpers (honour --dry-run) ─────────────────────────────────────────
run_rm() {  # run_rm <path> [-r]
    local path="$1"; local recursive="${2:-}"
    [[ -e "$path" || -L "$path" ]] || return 0
    if (( DRY_RUN )); then
        warn "would remove: $path"
    else
        rm -f $recursive "$path" && ok "removed: $path"
    fi
}

# ── 1. Stop any running daemon (project-local PID files) ──────────────────────
stop_daemons() {
    info "Stopping running brain daemons…"
    local found=0
    while IFS= read -r -d '' pidfile; do
        found=1
        local pid; pid="$(cat "$pidfile" 2>/dev/null || true)"
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            if (( DRY_RUN )); then
                warn "would kill daemon PID $pid (from $pidfile)"
            else
                kill "$pid" 2>/dev/null && ok "stopped daemon PID $pid"
            fi
        fi
    done < <(find "$SCAN_ROOT" -type d \( -name node_modules -o -name target -o -name .git \) -prune -o \
                   -name brain.pid -path '*/.brain/brain.pid' -print0 2>/dev/null)
    # Best-effort sweep for any stragglers.
    if command -v pkill &>/dev/null; then
        if (( DRY_RUN )); then
            pgrep -x brain-daemon &>/dev/null && warn "would pkill leftover brain-daemon processes"
        else
            pkill -x brain-daemon 2>/dev/null && ok "killed leftover brain-daemon processes" || true
        fi
    fi
    (( found )) || info "  no daemon PID files found."
}

# ── 2. Remove installed binaries & Claude output styles ───────────────────────
remove_binaries() {
    info "Removing installed binaries from $BIN_DIR…"
    run_rm "$BIN_DIR/brain"
    run_rm "$BIN_DIR/brain-daemon"

    info "Removing Claude Code output styles…"
    run_rm "$CLAUDE_STYLES_DIR/brain-eco.md"
    run_rm "$CLAUDE_STYLES_DIR/brain-eco-ultra.md"
}

# ── 3. Remove the global ~/.brain data directory ──────────────────────────────
remove_global_data() {
    info "Removing global data directory $GLOBAL_BRAIN_DIR…"
    run_rm "$GLOBAL_BRAIN_DIR" -r
}

# ── 4. Strip brain hooks from every .claude/settings.json ─────────────────────
clean_settings() {
    info "Cleaning brain hooks from .claude/settings.json files…"
    local cleaner found=0
    while IFS= read -r -d '' settings; do
        grep -q 'brain' "$settings" 2>/dev/null || continue
        found=1
        if (( DRY_RUN )); then
            warn "would strip brain hooks from: $settings"
            continue
        fi
        if command -v jq &>/dev/null; then
            local tmp; tmp="$(mktemp)"
            # Drop any hook group whose commands mention brain; prune emptied keys.
            jq '
              if .hooks then
                .hooks |= with_entries(
                  .value |= map(select(
                    [(.hooks // [])[].command // ""] | any(test("brain")) | not
                  ))
                )
                | .hooks |= with_entries(select(.value | length > 0))
                | if (.hooks | length) == 0 then del(.hooks) else . end
              else . end
            ' "$settings" > "$tmp" 2>/dev/null \
              && mv "$tmp" "$settings" \
              && ok "cleaned brain hooks: $settings" \
              || { rm -f "$tmp"; warn "could not auto-clean $settings — edit it manually."; }
        else
            warn "jq not installed — leaving $settings; remove brain hook entries by hand."
        fi
    done < <(find "$SCAN_ROOT" -type d \( -name node_modules -o -name target \) -prune -o \
                   -name settings.json -path '*/.claude/settings.json' -print0 2>/dev/null)
    (( found )) || info "  no settings.json with brain hooks found."
}

# ── 5. Remove per-project .brain/ dirs and generated hook scripts ─────────────
remove_project_state() {
    info "Removing per-project .brain/ directories and hook scripts…"
    local found=0
    while IFS= read -r -d '' braindir; do
        # Skip the source repo's own .brain unless we are purging the source.
        if [[ "$braindir" == "$SCRIPT_DIR/.brain" && $PURGE_SOURCE -eq 0 ]]; then
            : # still remove project state even in source; it's regenerable
        fi
        found=1
        run_rm "$braindir" -r
        local projroot; projroot="$(dirname "$braindir")"
        run_rm "$projroot/.claude/hooks/pre_prompt.sh"
        run_rm "$projroot/.claude/hooks/post_response.sh"
    done < <(find "$SCAN_ROOT" -type d \( -name node_modules -o -name target -o -name .git \) -prune -o \
                   -type d -name .brain -print0 2>/dev/null)
    (( found )) || info "  no project .brain/ directories found."
}

# ── 6. Optionally delete the source repository ────────────────────────────────
remove_source() {
    (( PURGE_SOURCE )) || { info "Keeping source repo at $SCRIPT_DIR (pass --purge-source to delete)."; return; }
    info "Deleting source repository $SCRIPT_DIR…"
    if (( DRY_RUN )); then
        warn "would remove source repo: $SCRIPT_DIR"
    else
        # cd out first so we don't delete the cwd from under ourselves.
        cd /tmp
        rm -rf "$SCRIPT_DIR" && ok "removed source repo: $SCRIPT_DIR"
    fi
}

# ── Confirmation ──────────────────────────────────────────────────────────────
echo
warn "This will REMOVE the Brain Engine from your system:"
printf "    binaries     %s/{brain,brain-daemon}\n" "$BIN_DIR"
printf "    styles       %s/brain-eco*.md\n"        "$CLAUDE_STYLES_DIR"
printf "    global data  %s  (config, providers, cache, logs, memory, models)\n" "$GLOBAL_BRAIN_DIR"
printf "    project data every .brain/ + brain hooks under %s\n" "$SCAN_ROOT"
(( PURGE_SOURCE )) && printf "    ${RED}source repo  %s  (DELETED)${RESET}\n" "$SCRIPT_DIR"
(( DRY_RUN ))      && printf "    ${YELLOW}(dry-run: nothing will actually be deleted)${RESET}\n"
echo

if (( ! ASSUME_YES && ! DRY_RUN )); then
    read -r -p "Proceed? Type 'yes' to continue: " reply
    [[ "$reply" == "yes" ]] || { info "Aborted."; exit 0; }
fi

# ── Run ───────────────────────────────────────────────────────────────────────
stop_daemons
remove_binaries
remove_global_data
clean_settings
remove_project_state
remove_source

echo
if (( DRY_RUN )); then
    ok "Dry-run complete — no changes were made."
else
    ok "Brain Engine removed."
    info "If $BIN_DIR was added to your PATH manually, remove that line from your shell profile."
fi
