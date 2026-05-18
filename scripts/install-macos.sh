#!/usr/bin/env bash
# scripts/install-macos.sh — Build + install nestty-macos as a real .app
# and install nestctl via `cargo install --path nestty-cli`.
#
# Companion to scripts/install-dev.sh (which is Linux-only — it does
# `cargo build --workspace`, and the workspace contains nestty-linux which
# does not build on macOS without GTK4).
#
# Why this script exists:
#   - The macOS GUI app builds via SwiftPM in nestty-macos/, not cargo.
#     Up to now, nestty-macos/run.sh was the only path, and it builds an
#     ephemeral debug bundle under .build/debug/ and `open -n`s it. There
#     was no way to install nestty as a real /Applications app.
#   - `cargo install nestty-cli` (crates.io) fails — the package is not
#     published. `cargo install --path .` from the repo root also fails
#     because the root manifest is a workspace, not a package. The
#     correct invocation is `cargo install --path nestty-cli`, which this
#     script wraps so the user does not need to memorize it.
#
# Usage:
#   ./scripts/install-macos.sh              # ~/Applications + ~/.cargo/bin (no sudo)
#   ./scripts/install-macos.sh --system     # /Applications + ~/.cargo/bin (sudo for /Applications)
#   ./scripts/install-macos.sh --no-build   # skip swift build (use existing .build/release/Nestty)
#   ./scripts/install-macos.sh --no-nestctl # skip cargo install of nestctl
#   ./scripts/install-macos.sh --no-nesttyd # skip cargo install of nesttyd (daemon)
#   ./scripts/install-macos.sh --no-plugins # skip building/installing plugin binaries
#   ./scripts/install-macos.sh --launch     # open the installed app afterwards
#
# Notes:
#   - nestctl + nesttyd always go to ~/.cargo/bin (cargo install's default).
#     If you want them in /usr/local/bin, run `sudo install -m755 \\
#     ~/.cargo/bin/{nestctl,nesttyd} /usr/local/bin/` after this script.
#   - This script kills any running Nestty instance so the binary can be
#     replaced. macOS holds an exclusive lock on a running .app's exec.
#   - First launch may show Gatekeeper warning if the .app is unsigned;
#     right-click → Open once, or `xattr -d com.apple.quarantine` (only
#     applies to downloaded apps; locally-built bundles do not carry the
#     quarantine xattr).

set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
    echo "this script is macOS-only; on Linux use scripts/install-dev.sh" >&2
    exit 2
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="Nestty.app"
DO_BUILD=true
SYSTEM_INSTALL=false
DO_NESTCTL=true
DO_NESTTYD=true
DO_PLUGINS=true
DO_LAUNCH=false

# macOS-buildable plugins. All first-party plugins now compile on macOS:
# - PR 4 added git (no platform-specific deps).
# - PR 5a added llm — proved `keyring` `apple-native` reaches Apple
#   Keychain at runtime.
# - PR 5b added calendar — validated the polling-daemon supervisor
#   lifecycle on macOS (background poller publishing
#   `calendar.event_imminent`). RPC actions still work without Google
#   OAuth creds thanks to `Config::minimal()` fallback.
# - kb / todo / bookmark formerly required Linux's `renameat2(RENAME_NOREPLACE)`;
#   the shared `nestty_core::fs_atomic` primitive now selects between
#   `renameat2` (Linux) and `renamex_np(RENAME_EXCL)` (Darwin), so all
#   three install and run on macOS.
# - slack / discord install fine; full functionality needs user-supplied
#   Slack `xoxb-` tokens / Discord bot tokens in Keychain (see plugin
#   READMEs). Without creds the plugins return RPC errors gracefully
#   rather than crashing the supervisor.
MACOS_PLUGINS=(echo git llm calendar kb todo bookmark slack discord jira)

while [[ $# -gt 0 ]]; do
    case "$1" in
        --system)      SYSTEM_INSTALL=true ; shift ;;
        --no-build)    DO_BUILD=false ; shift ;;
        --no-nestctl)  DO_NESTCTL=false ; shift ;;
        --no-nesttyd)  DO_NESTTYD=false ; shift ;;
        --no-plugins)  DO_PLUGINS=false ; shift ;;
        --launch)      DO_LAUNCH=true ; shift ;;
        -h|--help)
            sed -n '2,/^set -euo/p' "$0" | grep -E '^# ' | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "unknown flag: $1" >&2
            exit 2
            ;;
    esac
done

if $SYSTEM_INSTALL; then
    APP_DEST="/Applications"
    SUDO_APP="sudo"
else
    APP_DEST="$HOME/Applications"
    SUDO_APP=""
fi

# 1. Build the macOS app via SwiftPM (release config).
#    The Nestty executable links libnestty_ffi.a from the Rust staticlib crate;
#    SwiftPM cannot run cargo as a prebuild step from Package.swift, so we
#    invoke cargo here first. swift build's linker phase then picks up the
#    archive at $REPO_ROOT/target/release/libnestty_ffi.a via the
#    -L../target/release flag baked into Package.swift.
if $DO_BUILD; then
    echo "==> cargo build --release -p nestty-ffi -p nestty-term (Rust staticlibs for Swift FFI)"
    (cd "$REPO_ROOT" && cargo build --release -p nestty-ffi -p nestty-term)

    echo "==> swift build -c release (nestty-macos)"
    (cd "$REPO_ROOT/nestty-macos" && swift build -c release)
fi

BUILT_BIN="$REPO_ROOT/nestty-macos/.build/release/Nestty"
if [[ ! -x "$BUILT_BIN" ]]; then
    echo "error: $BUILT_BIN not found — drop --no-build, or run swift build -c release in nestty-macos/" >&2
    exit 1
fi

# 2. Stop any running instance so we can replace the bundle's executable.
pkill -x Nestty 2>/dev/null || true
sleep 0.3

# 3. Stage the bundle in a tmp dir so the install is atomic — the user
#    never sees a half-written .app at $APP_DEST.
STAGING_DIR="$(mktemp -d)"
trap 'rm -rf "$STAGING_DIR"' EXIT
STAGING="$STAGING_DIR/$APP_NAME"
CONTENTS="$STAGING/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"
mkdir -p "$MACOS" "$RESOURCES"
cp "$BUILT_BIN" "$MACOS/Nestty"

# Bundle icon. CFBundleIconFile expects the basename ("AppIcon") and
# Finder/Dock/Launchpad pull pixels from Resources/AppIcon.icns. The
# .icns is checked in (generated from assets/icons/nestty.png — see
# scripts/build-icons.sh) so swift build alone is enough to produce a
# fully-iconed bundle.
ICNS_SRC="$REPO_ROOT/nestty-macos/Resources/AppIcon.icns"
if [[ -f "$ICNS_SRC" ]]; then
    cp "$ICNS_SRC" "$RESOURCES/AppIcon.icns"
else
    echo "warn: $ICNS_SRC missing — bundle will fall back to the generic app icon" >&2
fi

# Info.plist — kept in sync with nestty-macos/run.sh by hand. Two copies is
# acceptable (Rule of Three); a third would mean extracting to a template.
cat > "$CONTENTS/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>Nestty</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>com.marshall.nestty</string>
    <key>CFBundleName</key>
    <string>nestty</string>
    <key>CFBundleDisplayName</key>
    <string>nestty</string>
    <key>CFBundleVersion</key>
    <string>0.1.0</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
</dict>
</plist>
EOF

# 4. Sign the staging bundle with a stable self-signed cert. Without
#    this, swift's ad-hoc linker signature gives a fresh cdhash on every
#    build, so macOS TCC treats each install as a different app and
#    re-prompts for every permission grant. Signing at staging means
#    --system installs don't need sudo for codesign.
"$REPO_ROOT/scripts/codesign-dev.sh" "$STAGING"

# 5. Install — replace any prior bundle in one rename so a partially-failed
#    install never leaves $APP_DEST in a broken state.
echo "==> installing $APP_NAME to $APP_DEST"
mkdir -p "$APP_DEST" 2>/dev/null || $SUDO_APP mkdir -p "$APP_DEST"
$SUDO_APP rm -rf "$APP_DEST/$APP_NAME"
$SUDO_APP mv "$STAGING" "$APP_DEST/$APP_NAME"

# 6. Install nestctl + nesttyd via cargo install (writes to ~/.cargo/bin).
#    Same rationale as nestctl: `cargo install <name>` fails (not on
#    crates.io) and `cargo install --path .` fails (workspace virtual
#    manifest), so we always pass `--path <crate-dir>`.
#
#    nesttyd is the background daemon (status bar, triggers, plugin
#    runtime). The Swift app auto-spawns it on launch when missing, so
#    a fresh install without nesttyd in PATH would warn-and-skip the
#    status bar / plugin features.
if $DO_NESTCTL; then
    echo "==> cargo install --path nestty-cli (nestctl → ~/.cargo/bin)"
    cargo install --path "$REPO_ROOT/nestty-cli"
fi
if $DO_NESTTYD; then
    echo "==> cargo install --path nestty-daemon (nesttyd → ~/.cargo/bin)"
    cargo install --path "$REPO_ROOT/nestty-daemon"
fi

# 7. Build + install macOS-buildable plugins. PluginSupervisor (PR 3) reads
#    ~/Library/Application Support/nestty/plugins/<name>/ at startup; we
#    cargo-build the binary and copy the manifest. Manifest's
#    `services.exec` is resolved against the plugin dir first, so we drop
#    the binary alongside plugin.toml so the supervisor finds it without
#    a $PATH dance.
PLUGIN_DEST="$HOME/Library/Application Support/nestty/plugins"
if $DO_PLUGINS; then
    mkdir -p "$PLUGIN_DEST"
    for name in "${MACOS_PLUGINS[@]}"; do
        crate="nestty-plugin-$name"
        src_manifest="$REPO_ROOT/plugins/$name/plugin.toml"
        if [[ ! -f "$src_manifest" ]]; then
            echo "skip plugin $name: $src_manifest missing"
            continue
        fi
        echo "==> cargo build --release -p $crate"
        (cd "$REPO_ROOT" && cargo build --release -p "$crate")

        bin_src="$REPO_ROOT/target/release/$crate"
        if [[ ! -x "$bin_src" ]]; then
            echo "warn  plugin $name: binary $bin_src not built — skipping" >&2
            continue
        fi

        plugin_dir="$PLUGIN_DEST/$name"
        mkdir -p "$plugin_dir"
        # Copy every loose file next to plugin.toml (manifest + panel.html
        # if any) so panel-bearing plugins land complete. Cargo.toml lives
        # in the same dir but is build-time only — exclude it.
        find "$REPO_ROOT/plugins/$name" -maxdepth 1 -type f ! -name 'Cargo.toml' \
            -exec cp -f {} "$plugin_dir/" \;
        # Copy (don't symlink) the binary so a `git clean` of target/ doesn't
        # silently break the install. Cheap — these binaries are small.
        cp -f "$bin_src" "$plugin_dir/$crate"
        chmod 755 "$plugin_dir/$crate"
        echo "ok    plugin $name → $plugin_dir/"
    done
fi

if $DO_LAUNCH; then
    open "$APP_DEST/$APP_NAME"
fi

cat <<EOF

Installed:
  $APP_DEST/$APP_NAME
EOF
if $DO_NESTCTL; then
    echo "  $HOME/.cargo/bin/nestctl"
fi
if $DO_NESTTYD; then
    echo "  $HOME/.cargo/bin/nesttyd"
fi
if $DO_PLUGINS; then
    echo "  $PLUGIN_DEST/{$(IFS=,; echo "${MACOS_PLUGINS[*]}")}"
fi
cat <<'EOF'

Next:
  - Launch the GUI: `open -a Nestty` (or Spotlight / Launchpad).
  - CLI helpers on the app binary itself:
      Nestty.app/Contents/MacOS/Nestty --version
      Nestty.app/Contents/MacOS/Nestty --config-path
      Nestty.app/Contents/MacOS/Nestty --init-config   # writes ~/.config/nestty/config.toml if missing
    (Many users alias `nestty` to that binary so `nestty --config-path` works.)
  - Verify a plugin is alive: `nestctl call echo.ping --params '{"hi":"there"}'`
  - Tail recent daemon events:    `nestctl recent`
EOF
