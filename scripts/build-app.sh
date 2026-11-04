#!/bin/bash
set -euo pipefail

# ── Build & assemble Fastab.app ──────────────────────────────────────
#
# Builds the Rust binaries and TypeScript frontend, then assembles a complete
# `build/Fastab.app` bundle. Does NOT install to /Applications or touch
# any system state — that is install.sh's job. This script is the single source
# of truth for how the .app is put together, shared by install.sh and CI.
#
# Output: build/Fastab.app  (ad-hoc code-signed)

APP_NAME="fastab"          # binary / process name (no spaces)
APP_DISPLAY="FasTab"       # localized, human-readable product name
APP_BUNDLE_NAME="Fastab.app" # stable installation path across display-name changes
BUNDLE_ID="app.fastab"
APP_CATEGORY="public.app-category.productivity"   # Finder / Launchpad "Developer Tools"
COPYRIGHT="${COPYRIGHT:-© 2026 Fastab contributors}"
DEFAULT_SPARKLE_APPCAST_URL="https://github.com/codeime/easy-complete/releases/latest/download/appcast.xml"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-12.0}"

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"

# Embed the checked-out commit in every binary, including same-version rebuilds.
# These values identify the build; disk bundle metadata is not proof of the
# identity of an older process that stayed mapped across an upgrade.
export AMAZON_Q_BUILD_HASH="${AMAZON_Q_BUILD_HASH:-$(git -C "$REPO_DIR" rev-parse HEAD)}"
export AMAZON_Q_BUILD_DATETIME="${AMAZON_Q_BUILD_DATETIME:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
export AMAZON_Q_BUILD_TARGET_TRIPLE="aarch64-apple-darwin"
export FASTAB_BUILD_RUN_ID="${FASTAB_BUILD_RUN_ID:-${GITHUB_RUN_ID:-local}}"

# Rust embeds the icons from the repository bundle and the IR compiler below
# reads that same directory. Refuse a redirected source tree before doing any
# build work; custom source/output directories are still supported by the
# standalone scripts, but cannot produce a coherent app through this entrypoint.
CANONICAL_SPECS_DIR="${REPO_DIR}/bundle/specs"
CANONICAL_SPECS_IR="${REPO_DIR}/bundle/specs-ir"
if [ -n "${BUNDLED_SPECS_DIR:-}" ] && [ "$BUNDLED_SPECS_DIR" != "$CANONICAL_SPECS_DIR" ]; then
  echo "error: build-app.sh requires BUNDLED_SPECS_DIR to be unset or exactly ${CANONICAL_SPECS_DIR}; unset it or use the standalone sync script for a custom output directory" >&2
  exit 1
fi
if [ -n "${EC_SPECS_SRC:-}" ] && [ "$EC_SPECS_SRC" != "$CANONICAL_SPECS_DIR" ]; then
  echo "error: build-app.sh requires EC_SPECS_SRC to be unset or exactly ${CANONICAL_SPECS_DIR}; unset it or run the standalone compiler for a custom source directory" >&2
  exit 1
fi
if [ -n "${EC_SPECS_IR:-}" ] && [ "$EC_SPECS_IR" != "$CANONICAL_SPECS_IR" ]; then
  echo "error: build-app.sh requires EC_SPECS_IR to be unset or exactly ${CANONICAL_SPECS_IR}; unset it or run the standalone compiler for a custom IR directory" >&2
  exit 1
fi

if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
  echo "error: Fastab release bundles must be built on Apple Silicon macOS" >&2
  exit 1
fi

VERSION=$(cargo metadata --no-deps --format-version 1 | python3 -c "import sys,json; print(next(pkg['version'] for pkg in json.load(sys.stdin)['packages'] if pkg['name'] == 'fastab_desktop'))")

FINAL_BUNDLE="${REPO_DIR}/build/${APP_BUNDLE_NAME}"
SPARKLE_APPCAST_URL="${SPARKLE_APPCAST_URL:-$DEFAULT_SPARKLE_APPCAST_URL}"
SPARKLE_AUTOMATIC_CHECKS="${SPARKLE_AUTOMATIC_CHECKS:-}"
if [ -z "$SPARKLE_AUTOMATIC_CHECKS" ]; then
  # GitHub's /releases/latest excludes prereleases. A beta build must not
  # poll the stable feed as if it were a beta update channel. Unsigned builds
  # also cannot install a Sparkle update, so avoid polling a missing appcast.
  if [ -n "${SPARKLE_PUBLIC_ED_KEY:-}" ] && [ -n "${SPARKLE_PRIVATE_ED_KEY:-}" ] && [[ "$VERSION" != *-* ]]; then
    SPARKLE_AUTOMATIC_CHECKS="true"
  else
    SPARKLE_AUTOMATIC_CHECKS="false"
  fi
fi
case "$SPARKLE_AUTOMATIC_CHECKS" in
  true) SPARKLE_AUTOMATIC_CHECKS_ENTRY="<true/>" ;;
  false) SPARKLE_AUTOMATIC_CHECKS_ENTRY="<false/>" ;;
  *) echo "error: SPARKLE_AUTOMATIC_CHECKS must be true or false" >&2; exit 1 ;;
esac

GREEN='\033[0;32m'; NC='\033[0m'
info() { echo -e "${GREEN}==>${NC} $*"; }

cd "$REPO_DIR"

mkdir -p "${REPO_DIR}/build"
BUILD_WORK_ROOT="$(mktemp -d "${REPO_DIR}/build/.specs-inputs.XXXXXX")"
STAGING_BUNDLE="${BUILD_WORK_ROOT}/${APP_BUNDLE_NAME}"
MACOS_DIR="${STAGING_BUNDLE}/Contents/MacOS"
RESOURCES_DIR="${STAGING_BUNDLE}/Contents/Resources"
FRAMEWORKS_DIR="${STAGING_BUNDLE}/Contents/Frameworks"
SPECS_IR_BUILD_SNAPSHOT="${BUILD_WORK_ROOT}/specs-ir"
BIN_BUILD_SNAPSHOT="${BUILD_WORK_ROOT}/bin"
BUILD_HELPER_PID=""
STOPPING_BUILD=0
PUBLISHING_BUILD=0
PRESERVE_BUILD_WORK=0
BUILD_COMPLETED=0
BUILD_WORK_IDENTITY="$(/usr/bin/stat -f '%d:%i' "$BUILD_WORK_ROOT")"

cleanup_build_work() {
  if [ "$PRESERVE_BUILD_WORK" -eq 1 ] || [ "$BUILD_COMPLETED" -eq 0 ]; then
    echo "warning: preserving build work directory after failed or interrupted build: $BUILD_WORK_ROOT" >&2
    return
  fi
  local current_identity
  current_identity="$(/usr/bin/stat -f '%d:%i' "$BUILD_WORK_ROOT" 2>/dev/null || true)"
  if [ "$current_identity" != "$BUILD_WORK_IDENTITY" ]; then
    echo "warning: refusing to remove replaced build work directory: $BUILD_WORK_ROOT" >&2
    return
  fi
  rm -rf -- "$BUILD_WORK_ROOT"
}

stop_build_work() {
  local signal="$1"
  local status="$2"
  local helper_pid="$BUILD_HELPER_PID"
  # Bash does not recursively run the same signal trap while it is waiting
  # inside that trap. Ignore subsequent INT/TERM immediately; the Node helper
  # arms its own five-second process-group SIGKILL fallback on the first one.
  trap '' INT TERM
  if [ "$STOPPING_BUILD" -eq 1 ]; then
    return
  fi
  STOPPING_BUILD=1
  if [ -z "$helper_pid" ]; then
    # Close the tiny `command &` -> `$!` assignment window. This script runs
    # only one background helper at a time, so the final active job is ours.
    local running_jobs
    running_jobs="$(jobs -pr)"
    if [ -n "$running_jobs" ]; then
      helper_pid="${running_jobs##*$'\n'}"
    fi
  fi
  if [ "$PUBLISHING_BUILD" -eq 1 ] || [ -n "$helper_pid" ]; then
    PRESERVE_BUILD_WORK=1
  fi
  if [ -n "$helper_pid" ]; then
    if kill -0 "$helper_pid" 2>/dev/null; then
      kill -s "$signal" "$helper_pid" 2>/dev/null || true
    fi
    wait "$helper_pid" 2>/dev/null || true
    BUILD_HELPER_PID=""
  fi
  cleanup_build_work
  trap - EXIT INT TERM
  exit "$status"
}

trap cleanup_build_work EXIT
trap 'stop_build_work INT 130' INT
trap 'stop_build_work TERM 143' TERM
# BUILD_COMPLETED stays zero until publication succeeds. Therefore every
# foreground helper/assembly error reaches EXIT with the work root preserved,
# including failures that happen after the background helper has been reaped.

# ── 0. Verify bundled specs ───────────────────────────────────────────────────
# Keep the source bundle and the native binary's embedded icons in lockstep.
# The default dependency path is checked without rewriting the tracked bundle;
# a missing/stale manifest or changed source/config causes a deterministic
# resync before cargo can embed stale icons. Explicit npm/CDN source overrides
# retain their historical sync behavior.
SPECS_IR_PUBLISHED_BY_SYNC=0
if [ "${BUNDLED_SPECS_SOURCE:-dependency}" = "dependency" ]; then
  if node "${REPO_DIR}/scripts/sync-bundled-specs.mjs" --check; then
    info "Bundled specs are fresh."
  else
    info "Bundled specs are missing or stale; syncing them now..."
    node "${REPO_DIR}/scripts/sync-bundled-specs.mjs"
    SPECS_IR_PUBLISHED_BY_SYNC=1
  fi
else
  info "Syncing bundled specs from BUNDLED_SPECS_SOURCE=${BUNDLED_SPECS_SOURCE}..."
  node "${REPO_DIR}/scripts/sync-bundled-specs.mjs"
  SPECS_IR_PUBLISHED_BY_SYNC=1
fi

# Compile the runtime IR before Rust so the publication lock below can pin one
# source/IR generation across rustc's include_bytes! reads and the resource
# snapshot copied into the final app.
if [ "$SPECS_IR_PUBLISHED_BY_SYNC" = "0" ]; then
  info "Compiling spec IR..."
  node "${REPO_DIR}/scripts/compile-spec-ir.mjs"
else
  info "Using the source/IR pair published together by the spec sync..."
fi
if [ ! -f "${REPO_DIR}/bundle/specs-ir/index.json" ]; then
  echo "error: spec IR compile did not write index.json" >&2
  exit 1
fi
if [ -e "${REPO_DIR}/bundle/specs-ir/hooks" ] || [ -e "${REPO_DIR}/bundle/specs-ir/source-modules" ] || [ -e "${REPO_DIR}/bundle/specs-ir/hook-modules.json" ]; then
  echo "error: spec IR still contains leftover runtime JS (hooks/, source-modules/, or hook-modules.json)" >&2
  exit 1
fi
if [ ! -f "${REPO_DIR}/bundle/specs-ir/typed-hooks.json" ]; then
  echo "error: spec IR compile did not write typed-hooks.json" >&2
  exit 1
fi

# ── 1. Build ──────────────────────────────────────────────────────────────────
# Distribution build profile (size/perf-optimized). Override with
# CARGO_PROFILE=release for a faster local iteration build. The helper reads
# Cargo's JSON artifact messages and copies this invocation's exact binaries
# into BIN_BUILD_SNAPSHOT, so target-dir and build-target overrides cannot make
# the bundle pick up an old path.
CARGO_PROFILE="${CARGO_PROFILE:-dist}"

info "Auditing and building Rust binaries from one locked spec generation (profile: ${CARGO_PROFILE})..."
# POSTHOG_ENDPOINT and POSTHOG_API_KEY are baked in at compile time via option_env!().
# Set both before running this script to enable telemetry, e.g.:
#   POSTHOG_ENDPOINT=https://analytics.example.com/capture/ \
#   POSTHOG_API_KEY=phc_xxx \
#   ./scripts/build-app.sh
# Either being unset disables telemetry silently.
POSTHOG_ENDPOINT="${POSTHOG_ENDPOINT:-}" \
POSTHOG_API_KEY="${POSTHOG_API_KEY:-}" \
node "${REPO_DIR}/scripts/build-spec-inputs.mjs" \
  --profile "$CARGO_PROFILE" \
  --snapshot "$SPECS_IR_BUILD_SNAPSHOT" &
BUILD_HELPER_PID=$!
set +e
wait "$BUILD_HELPER_PID"
BUILD_HELPER_STATUS=$?
if [ "$BUILD_HELPER_STATUS" -ne 0 ]; then
  # The helper refuses to clean a snapshot whose inode changed. Preserve the
  # outer work root too, otherwise recursive shell cleanup would undo that
  # fail-safe decision and could delete a raced-in directory.
  PRESERVE_BUILD_WORK=1
fi
BUILD_HELPER_PID=""
set -e
if [ "$BUILD_HELPER_STATUS" -ne 0 ]; then
  exit "$BUILD_HELPER_STATUS"
fi

info "Assembling '${APP_BUNDLE_NAME}' (${APP_DISPLAY})..."
mkdir -p "$MACOS_DIR"
mkdir -p "${RESOURCES_DIR}/themes"

info "Embedding Sparkle.framework..."
SPARKLE_FRAMEWORK="${SPARKLE_FRAMEWORK:-$("${REPO_DIR}/scripts/fetch-sparkle.sh")}"
[ -d "$SPARKLE_FRAMEWORK" ] || { echo "error: Sparkle framework not found: $SPARKLE_FRAMEWORK" >&2; exit 1; }

if [ "${SKIP_NOTICES_CHECK:-}" = "1" ]; then
  info "Skipping third-party notices check (SKIP_NOTICES_CHECK=1)"
else
  node "${REPO_DIR}/scripts/generate-third-party-notices.mjs" --check "${REPO_DIR}/THIRD_PARTY_NOTICES.txt"
fi

mkdir -p "$FRAMEWORKS_DIR"
cp -R "$SPARKLE_FRAMEWORK" "$FRAMEWORKS_DIR/"

# Sparkle ships as a universal framework. The application is ARM64-only, so
# remove Intel slices before signing and packaging the bundle.
while IFS= read -r -d '' binary; do
  if file -b "$binary" | grep -q "Mach-O universal binary"; then
    thinned="${binary}.arm64"
    lipo "$binary" -thin arm64 -output "$thinned"
    chmod "$(stat -f '%Lp' "$binary")" "$thinned"
    mv "$thinned" "$binary"
  fi
done < <(find "${FRAMEWORKS_DIR}/Sparkle.framework" -type f -print0)

SPARKLE_PUBLIC_KEY_ENTRY=""
if [ -n "${SPARKLE_PUBLIC_ED_KEY:-}" ]; then
  read -r -d '' SPARKLE_PUBLIC_KEY_ENTRY <<PLIST || true
    <key>SUPublicEDKey</key>
    <string>${SPARKLE_PUBLIC_ED_KEY}</string>
PLIST
fi

# InstallerLauncher XPC service requires Developer ID signing (a real TeamIdentifier).
# Ad-hoc builds must disable it or Sparkle reports "connecting to the installer" errors.
SPARKLE_INSTALLER_LAUNCHER="<false/>"
if [ -n "${SIGNING_IDENTITY:-}" ]; then
  SPARKLE_INSTALLER_LAUNCHER="<true/>"
fi

read -r -d '' SPARKLE_PLIST_ENTRIES <<PLIST || true
    <key>SUFeedURL</key>
    <string>${SPARKLE_APPCAST_URL}</string>
    <key>SUEnableAutomaticChecks</key>
    ${SPARKLE_AUTOMATIC_CHECKS_ENTRY}
    <key>SUScheduledCheckInterval</key>
    <integer>86400</integer>
${SPARKLE_PUBLIC_KEY_ENTRY}    <key>SUEnableInstallerLauncherService</key>
    ${SPARKLE_INSTALLER_LAUNCHER}
PLIST

node "${REPO_DIR}/scripts/build-spec-inputs.mjs" \
  --verify-snapshot "$SPECS_IR_BUILD_SNAPSHOT"
cp "${BIN_BUILD_SNAPSHOT}/${APP_NAME}" "$MACOS_DIR/"
cp "${BIN_BUILD_SNAPSHOT}/ftab"        "$MACOS_DIR/"
cp "${BIN_BUILD_SNAPSHOT}/fterm"      "$MACOS_DIR/"
# fterm stays resident once per terminal tab and binds nothing outside
# libSystem; crates/fastab_term/build.rs drops the unused framework links.
# Each one that comes back costs every tab its initializers and data pages.
fterm_links="$(otool -L "${MACOS_DIR}/fterm" | tail -n +2 | awk '{print $1}')"
if [ "$fterm_links" != "/usr/lib/libSystem.B.dylib" ]; then
  echo "error: fterm links more than libSystem:" >&2
  echo "$fterm_links" >&2
  exit 1
fi
# Older shell sessions still resolve the previous PTY entry point.
ln -s fterm "${MACOS_DIR}/fastabterm"

cp themes/*.json                       "${RESOURCES_DIR}/themes/"
# Only specs-ir ships. bundle/specs is build-time input: it feeds the IR compiler
# above, and fastab_gpui embeds its icons with include_bytes!. The .app never reads it.
node "${REPO_DIR}/scripts/build-spec-inputs.mjs" \
  --verify-snapshot "$SPECS_IR_BUILD_SNAPSHOT"
if [ -d "$SPECS_IR_BUILD_SNAPSHOT" ]; then
  cp -R "$SPECS_IR_BUILD_SNAPSHOT"    "${RESOURCES_DIR}/specs-ir"
fi
if [ -e "${RESOURCES_DIR}/specs-ir/hooks" ] || [ -e "${RESOURCES_DIR}/specs-ir/source-modules" ] || [ -e "${RESOURCES_DIR}/specs-ir/hook-modules.json" ]; then
  echo "error: app bundle specs-ir still contains leftover runtime JS" >&2
  exit 1
fi
if [ ! -f "${RESOURCES_DIR}/specs-ir/typed-hooks.json" ]; then
  echo "error: app bundle is missing specs-ir/typed-hooks.json" >&2
  exit 1
fi
EC_SPECS_IR="${RESOURCES_DIR}/specs-ir" \
  node "${REPO_DIR}/scripts/spec-pair.mjs" --ir-only

LICENSES_DIR="${RESOURCES_DIR}/Licenses"
mkdir -p "$LICENSES_DIR"
cp LICENSE NOTICE THIRD_PARTY_NOTICES.txt "$LICENSES_DIR/"

"${REPO_DIR}/scripts/verify-license-bundle.sh" "$STAGING_BUNDLE"

# Input Method helper app
IM_APP="${STAGING_BUNDLE}/Contents/Helpers/FastabInputMethod.app"
mkdir -p "${IM_APP}/Contents/MacOS"
mkdir -p "${IM_APP}/Contents/Resources"
node "${REPO_DIR}/scripts/build-spec-inputs.mjs" \
  --verify-snapshot "$SPECS_IR_BUILD_SNAPSHOT"
cp "${BIN_BUILD_SNAPSHOT}/fastab_input_method" "${IM_APP}/Contents/MacOS/"
cp "crates/fastab_input_method/Info.plist" "${IM_APP}/Contents/"
cp crates/fastab_input_method/resources/*  "${IM_APP}/Contents/Resources/" 2>/dev/null || true
node "${REPO_DIR}/scripts/build-spec-inputs.mjs" \
  --verify-snapshot "$SPECS_IR_BUILD_SNAPSHOT"

while IFS= read -r -d '' binary; do
  if file -b "$binary" | grep -q "Mach-O"; then
    archs="$(lipo -archs "$binary")"
    if [ "$archs" != "arm64" ]; then
      echo "error: non-ARM64 binary in app bundle: $binary ($archs)" >&2
      exit 1
    fi
  fi
done < <(find "$STAGING_BUNDLE" -type f -print0)

# The Dock icon is icon.icns, painted from assets/logo.svg. A layered
# icon catalog would replace that file on macOS 26 and was showing only
# the background fill, so this bundle does not compile or copy one.
cat > "${STAGING_BUNDLE}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID}</string>
    <key>CFBundleName</key>
    <string>${APP_BUNDLE_NAME%.app}</string>
    <key>CFBundleDisplayName</key>
    <string>${APP_BUNDLE_NAME%.app}</string>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>${APP_NAME}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSApplicationCategoryType</key>
    <string>${APP_CATEGORY}</string>
    <key>LSMinimumSystemVersion</key>
    <string>${MACOSX_DEPLOYMENT_TARGET}</string>
    <key>NSHumanReadableCopyright</key>
    <string>${COPYRIGHT}</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>LSUIElement</key>
    <true/>
    <!--
      Freed heap goes back to the system, about 25 MB of idle footprint. It has
      to be set before main, and the app must not re-exec itself to do that: on
      macOS 27 that loses its menu bar icon. See fastab_util::malloc.
    -->
    <key>LSEnvironment</key>
    <dict>
      <key>MallocSpaceEfficient</key>
      <string>1</string>
    </dict>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
    <!--
      macOS Tahoe spawns an "AutoFill (Fastab)" helper that heuristically
      scans text fields for one-time codes. This app never marks fields as
      one-time-code, so the helper is pure overhead. Documented Apple key:
      https://developer.apple.com/documentation/bundleresources/information-property-list/nsautofillrequirestextcontenttypeforonetimecodeonmac
    -->
    <key>NSAutoFillRequiresTextContentTypeForOneTimeCodeOnMac</key>
    <true/>
    <key>CFBundleIconFile</key>
    <string>icon</string>
    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleURLName</key>
            <string>${APP_DISPLAY} URL</string>
            <key>CFBundleURLSchemes</key>
            <array>
                <string>fastab</string>
            </array>
        </dict>
    </array>
${SPARKLE_PLIST_ENTRIES}
</dict>
</plist>
PLIST

# Finder uses localized names only when the base name matches the bundle path.
# Keep that path stable; the development language supplies the name in all locales.
mkdir -p "${RESOURCES_DIR}/en.lproj"
cat > "${RESOURCES_DIR}/en.lproj/InfoPlist.strings" <<STRINGS
"CFBundleName" = "${APP_DISPLAY}";
"CFBundleDisplayName" = "${APP_DISPLAY}";
STRINGS

# Copy app icon to Resources. This is the only app icon in the bundle.
cp "${REPO_DIR}/crates/fastab_desktop/icons/icon.icns" "${RESOURCES_DIR}/icon.icns"

# ── 3. Ad-hoc code sign ───────────────────────────────────────────────────────
# Release builds replace this with Developer ID signing in CI.
info "Ad-hoc code signing..."
codesign --force --deep --sign - "${FRAMEWORKS_DIR}/Sparkle.framework" 2>/dev/null || true
codesign --force --deep --sign - "${IM_APP}" 2>/dev/null || true
codesign --force --deep --sign - "${STAGING_BUNDLE}" 2>/dev/null || true

ATOMIC_SWAP_HELPER="${BUILD_WORK_ROOT}/atomic-swap-darwin"
/usr/bin/xcrun clang \
  -Os -Wall -Wextra -Werror \
  -mmacosx-version-min=12.0 \
  "${REPO_DIR}/scripts/atomic-swap-darwin.c" \
  -o "$ATOMIC_SWAP_HELPER"

PUBLISHING_BUILD=1
node "${REPO_DIR}/scripts/publish-app-bundle.mjs" \
  --staging "$STAGING_BUNDLE" \
  --final "$FINAL_BUNDLE" \
  --swap-helper "$ATOMIC_SWAP_HELPER" &
BUILD_HELPER_PID=$!
set +e
wait "$BUILD_HELPER_PID"
PUBLISH_HELPER_STATUS=$?
if [ "$PUBLISH_HELPER_STATUS" -ne 0 ]; then
  # Publication failures are fail-safe: a non-cooperating process may have
  # raced a pathname into the work root. Never recursively clean that tree.
  PRESERVE_BUILD_WORK=1
fi
BUILD_HELPER_PID=""
set -e
if [ "$PUBLISH_HELPER_STATUS" -ne 0 ]; then
  exit "$PUBLISH_HELPER_STATUS"
fi
PUBLISHING_BUILD=0
BUILD_COMPLETED=1

info "Built: ${FINAL_BUNDLE}"
