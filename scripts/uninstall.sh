#!/bin/bash
set -euo pipefail

# ── Fastab macOS uninstaller ───────────────────────────────────────────────
# Fastab-only. Easy Complete is a sibling product and is left installed.

APP_NAME="fastab"
APP_DISPLAY="FasTab"
APP_BUNDLE_NAME="Fastab.app"
BUNDLE_ID="app.fastab"
IME_BUNDLE_ID="app.fastab.inputmethod"

APP_BUNDLE="/Applications/${APP_BUNDLE_NAME}"
LOCAL_BIN="${HOME}/.local/bin"
LAUNCH_AGENTS="${HOME}/Library/LaunchAgents"
PLIST_PATH="${LAUNCH_AGENTS}/${BUNDLE_ID}.plist"
INPUT_METHODS_DIR="${HOME}/Library/Input Methods"
IME_SYMLINK="${INPUT_METHODS_DIR}/FastabInputMethod.app"
APP_SUPPORT="${HOME}/Library/Application Support/${APP_NAME}"
CACHE_DIR="${HOME}/Library/Caches/${APP_NAME}"
TMP_ROOT="${TMPDIR:-/tmp}"
TMP_ROOT="${TMP_ROOT%/}"

GREEN='\033[0;32m'; YELLOW='\033[0;33m'; RED='\033[0;31m'; NC='\033[0m'
info()  { echo -e "${GREEN}==>${NC} $*"; }
warn()  { echo -e "${YELLOW}==>${NC} $*"; }
error() { echo -e "${RED}==>${NC} $*" >&2; }

# ── Confirm ───────────────────────────────────────────────────────────────────
if [[ "${1:-}" != "--yes" ]]; then
  echo ""
  warn "This will completely remove ${APP_DISPLAY} and all its data."
  echo "  • /Applications/${APP_BUNDLE_NAME}"
  echo "  • ${IME_SYMLINK}"
  echo "  • ${PLIST_PATH}"
  echo "  • ${LOCAL_BIN}/ftab, FasTab's fterm/fastabterm links, and <shell> (fterm/fastabterm) copies"
  echo "  • ${APP_SUPPORT}/"
  echo "  • ${CACHE_DIR}/"
  echo "  • ${TMP_ROOT}/ftablog"
  echo "  • FasTab shell integration lines in ~/.zshrc / ~/.bashrc / ~/.config/fish/config.fish"
  echo "  • FasTab SSH Include in ~/.ssh/config"
  echo ""
  echo "  Easy Complete is not touched."
  echo ""
  read -r -p "Continue? [y/N] " confirm
  [[ "${confirm}" =~ ^[Yy]$ ]] || { echo "Aborted."; exit 0; }
fi

# ── 0. Telemetry (best-effort; reporting is off in this product) ─────────────
if command -v ftab &>/dev/null; then
  ftab telemetry track app_uninstalled 2>/dev/null || true
fi

# ── 1. Uninstall integrations via CLI (must run before binary is removed) ─────
info "Uninstalling input method integration..."
ime_cli_uninstalled=0
if command -v ftab &>/dev/null; then
  if ftab integrations uninstall input-method 2>/dev/null; then
    ime_cli_uninstalled=1
  fi
fi

info "Uninstalling shell integration..."
if command -v ftab &>/dev/null; then
  ftab integrations uninstall dotfiles 2>/dev/null || true
fi

info "Uninstalling SSH integration..."
if command -v ftab &>/dev/null; then
  ftab integrations uninstall ssh 2>/dev/null || true
fi

# ── 2. Kill running processes ─────────────────────────────────────────────────
info "Stopping processes..."
if [[ -x "${APP_BUNDLE}/Contents/MacOS/${APP_NAME}" ]]; then
  "${APP_BUNDLE}/Contents/MacOS/${APP_NAME}" --unregister-login-item 2>/dev/null || true
fi
pkill -x "${APP_NAME}"       2>/dev/null || true
pkill -f "FastabInputMethod.app/Contents/MacOS/fastab_input_method" 2>/dev/null || true
pkill -f "FastabInputMethod.app/Contents/MacOS/fig_input_method" 2>/dev/null || true
# fterm is left running on purpose. Each one owns a terminal tab's PTY, so
# killing it closes that tab and whatever runs in it, and this script usually
# runs in such a tab: the hangup ended it before the bundle, CLI links and data
# below were removed. With the desktop gone an fterm only passes bytes through,
# it cannot recreate the deleted data directory (that happens once, at tab
# start), and it exits when its tab closes.
sleep 0.5

# ── 3. Remove login item and Fastab LaunchAgent ──────────────────────────────
info "Removing login startup entries..."
uid="$(id -u)"
launchctl bootout "gui/${uid}/${BUNDLE_ID}" 2>/dev/null || true
if [[ -f "$PLIST_PATH" ]]; then
  launchctl unload "$PLIST_PATH" 2>/dev/null || true
  rm -f "$PLIST_PATH"
fi

# ── 4. Remove IME symlink ──────────────────────────────────────────────────────
info "Removing Input Method..."
if [[ -L "$IME_SYMLINK" || -d "$IME_SYMLINK" ]]; then
  rm -rf "$IME_SYMLINK"
fi

# `ftab integrations uninstall input-method` drops our palette entries
# in-process. Do not fall back to `defaults export`/`import` of the whole
# HIToolbox domain: that race is what dropped Easy Complete's palette when
# install ran two writers. A leftover Fastab row after the CLI is gone is
# inert, so only say which of the two happened.
if [[ "${ime_cli_uninstalled}" -eq 1 ]]; then
  info "Input Method palette entries were removed in-process (no HIToolbox domain rewrite)."
else
  warn "ftab could not finish uninstalling the Input Method; any leftover palette entry is inert."
fi

# ── 5. Remove app bundle ───────────────────────────────────────────────────────
info "Removing /Applications/${APP_BUNDLE_NAME}..."
rm -rf "$APP_BUNDLE"

# ── 6. Remove CLI symlinks ─────────────────────────────────────────────────────
info "Removing CLI symlinks..."
rm -f "${LOCAL_BIN}/ftab"
for name in fterm fastabterm; do
  link="${LOCAL_BIN}/${name}"
  if [[ -L "${link}" ]]; then
    case "$(readlink "${link}")" in
      "${APP_BUNDLE}/Contents/MacOS/fterm"|"${APP_BUNDLE}/Contents/MacOS/fastabterm") rm -f "${link}" ;;
    esac
  fi
done
# Desktop launch copies `zsh (fterm)` (and bash / fish / nu) for `exec -a`.
# Remove copies from both naming generations.
for shell in bash zsh fish nu; do
  rm -f "${LOCAL_BIN}/${shell} (fterm)"
  rm -f "${LOCAL_BIN}/${shell} (fastabterm)"
done

# ── 7. Fallback shell integration cleanup (in case the CLI was already removed)
# ftab integrations uninstall dotfiles / ssh were already called in step 1.
# This fallback removes only Fastab lines.
info "Verifying shell integration removal..."

# Puts "$2" in place of "$1"'s contents and removes "$2". The file is written
# through, not renamed over: `mv` of a temp file replaced a dotfile manager's
# symlink (stow, chezmoi, yadm) with a plain copy and reset the mode to
# mktemp's 0600.
write_back() {
  local file="$1" new="$2"
  if ! cat "$new" > "$file"; then
    warn "Could not update ${file}; remove the FasTab lines from it by hand."
  fi
  rm -f "$new"
}

# Writes the lines of "$2" that do not match "$1" to "$3". Fails only when the
# file could not be read: grep's status 1 just means no line was left, while
# its empty output after a read error must never be written back.
without_matching_lines() {
  local pattern="$1" file="$2" out="$3" status=0
  grep -Ev "$pattern" "$file" > "$out" || status=$?
  [[ "$status" -le 1 ]]
}

SHELL_INTEGRATION_LINES='Fastab (pre|post) block|fastab/shell/(zshrc|zprofile|bashrc|bash_profile)\.(pre|post)\.(zsh|bash)|eval "\$\((~/.local/bin/)?ftab init |eval \((~/.local/bin/)?ftab init |\[ -x ~/.local/bin/ftab \] && eval |command -v ftab >/dev/null 2>&1 && eval '

strip_shell_integration_fallback() {
  local rc_file="$1"
  [[ -f "$rc_file" ]] || return 0
  # Nothing of ours left: leave the file exactly as it is.
  grep -Eq "$SHELL_INTEGRATION_LINES" "$rc_file" || return 0

  local tmp
  tmp="$(mktemp)"
  if without_matching_lines "$SHELL_INTEGRATION_LINES" "$rc_file" "$tmp"; then
    write_back "$rc_file" "$tmp"
  else
    rm -f "$tmp"
    warn "Could not read ${rc_file}; left as it is."
  fi
}

strip_shell_integration_fallback "${HOME}/.zshrc"
strip_shell_integration_fallback "${HOME}/.zprofile"
strip_shell_integration_fallback "${HOME}/.bashrc"
strip_shell_integration_fallback "${HOME}/.bash_profile"
strip_shell_integration_fallback "${HOME}/.config/fish/config.fish"

# Fish dedicated conf files are still named 00_fig_pre.fish / 99_fig_post.fish.
# Easy Complete / Amazon Q / Fig use the same names. Always strip Fastab hook
# lines only — never delete the file while any other content remains.
FISH_CONF_LINES='ftab init|/\.local/bin/ftab|command -v ftab >/dev/null|command -qv ftab |fastab/shell/|^[[:space:]]*# Fastab '

maybe_remove_fastab_fish_conf() {
  local path="$1"
  [[ -f "$path" ]] || return 0
  grep -Eq "$FISH_CONF_LINES" "$path" || return 0

  local tmp
  tmp="$(mktemp)"
  if ! without_matching_lines "$FISH_CONF_LINES" "$path" "$tmp"; then
    rm -f "$tmp"
    warn "Could not read ${path}; left as it is."
  elif ! grep -Eq '[^[:space:]]' "$tmp"; then
    rm -f "$path" "$tmp"
  else
    write_back "$path" "$tmp"
  fi
}
maybe_remove_fastab_fish_conf "${HOME}/.config/fish/conf.d/00_fig_pre.fish"
maybe_remove_fastab_fish_conf "${HOME}/.config/fish/conf.d/99_fig_post.fish"

# Fastab's ~/.ssh/config block Includes Application Support/fastab/ssh. If that
# file is gone and the Include stays, OpenSSH errors on every ssh. Strip only
# the Fastab comment + Match all + fastab/ssh Include — sibling products keep
# their own blocks.
strip_fastab_ssh_config() {
  local ssh_config="${HOME}/.ssh/config"
  [[ -f "$ssh_config" ]] || return 0
  grep -q '^# Fastab SSH Integration' "$ssh_config" || return 0

  local tmp
  tmp="$(mktemp)"
  if awk '
    function flush() {
      if (held != "") print held
      held = ""
      pending = 0
    }
    /^# Fastab SSH Integration/ { flush(); held = $0; pending = 1; next }
    pending == 1 && $0 ~ /^Match all[[:space:]]*$/ { held = held "\n" $0; pending = 2; next }
    pending == 2 && $0 ~ /Include/ && $0 ~ /fastab\/ssh/ { held = ""; pending = 0; next }
    { flush(); print }
    END { flush() }
  ' "$ssh_config" > "$tmp"; then
    write_back "$ssh_config" "$tmp"
  else
    rm -f "$tmp"
    warn "Could not read ${ssh_config}; left as it is."
  fi
}
strip_fastab_ssh_config

# ── 8. Remove application data ────────────────────────────────────────────────
info "Removing application data..."
rm -rf "$APP_SUPPORT"
rm -rf "$CACHE_DIR"

# IPC sockets / logs live under the process temp dir, not ~/.local/share.
rm -rf "${TMP_ROOT}/fastabrun" "${TMP_ROOT}/ftablog" 2>/dev/null || true
rm -rf /tmp/fastabrun /tmp/ftablog 2>/dev/null || true

# Preferences
defaults delete "$BUNDLE_ID"          2>/dev/null || true
defaults delete "$IME_BUNDLE_ID"      2>/dev/null || true

# Accessibility grant — drop the now-dead TCC entry so it doesn't linger in
# System Settings pointing at a removed binary.
tccutil reset Accessibility "$BUNDLE_ID" 2>/dev/null || true

# Keychain entries (best-effort)
security delete-generic-password -s "$BUNDLE_ID" 2>/dev/null || true

# ── Done ───────────────────────────────────────────────────────────────────────
echo ""
info "FasTab has been fully uninstalled."
echo ""
echo "  To remove the ~/.local/bin directory itself (if empty):"
echo "    rmdir ~/.local/bin 2>/dev/null"
echo ""
echo "  Reload your shell to apply PATH changes:"
echo "    exec \$SHELL"
echo ""
echo "  Terminal tabs opened before the uninstall keep the old ${APP_DISPLAY} wrapper"
echo "  (shown as \"zsh (fterm)\") until you close them; new tabs start without it."
