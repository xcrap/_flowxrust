#!/usr/bin/env bash
# `fx` — build, test, and run FlowX.
#
# Every target in this workspace is a library except `flowx`, which is a thin
# binary. `cargo build` alone builds every library, so the interesting verbs
# below are about producing and launching the app bundle.

set -euo pipefail

BUNDLE_ID="com.flowx.rust"
DEV_BUNDLE_ID="com.flowx.rust.dev"
CONFIG="${FX_CONFIG:-debug}"

case "$CONFIG" in
  debug|release) ;;
  *) echo "Unsupported FX_CONFIG: $CONFIG (expected debug or release)" >&2; exit 2 ;;
esac

# macOS ships Bash 3.2, whose nounset treats an empty array as unbound.
# Branch here instead of expanding an optional array at every Cargo call.
profiled_cargo() {
  local verb="$1"
  shift
  if [ "$CONFIG" = "release" ]; then
    cargo "$verb" --release "$@"
  else
    cargo "$verb" "$@"
  fi
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

BUNDLE_DIR="target/$CONFIG/bundle"

bundle_id() {
  if [ "$CONFIG" = "release" ]; then echo "$BUNDLE_ID"; else echo "$DEV_BUNDLE_ID"; fi
}

app_name() {
  if [ "$CONFIG" = "release" ]; then echo "FlowX"; else echo "FlowX-Dev"; fi
}

# Assemble a minimal .app bundle around the compiled binary. Rust has no
# built-in bundler, and the app needs a real bundle to own a Dock icon, a menu
# bar, and an application-support directory.
make_bundle() {
  local name app
  name="$(app_name)"
  app="$BUNDLE_DIR/$name.app"

  rm -rf "$app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

  cp "target/$CONFIG/flowx" "$app/Contents/MacOS/$name"
  cp LICENSE NOTICE "$app/Contents/Resources/"
  cp -R licenses "$app/Contents/Resources/licenses"

  cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>$name</string>
    <key>CFBundleDisplayName</key><string>$name</string>
    <key>CFBundleExecutable</key><string>$name</string>
    <key>CFBundleIdentifier</key><string>$(bundle_id)</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>LSMinimumSystemVersion</key><string>14.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSAppTransportSecurity</key>
    <dict>
        <key>NSAllowsLocalNetworking</key><true/>
    </dict>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
</dict>
</plist>
PLIST

  echo "$app"
}

case "${1:-dev}" in
  build)
    profiled_cargo build --workspace --all-targets
    ;;

  bundle)
    profiled_cargo build -p flowx --bin flowx
    make_bundle
    ;;

  run|dev)
    profiled_cargo run -p flowx --bin flowx
    ;;

  launch)
    profiled_cargo build -p flowx --bin flowx
    app="$(make_bundle)"
    # Reuse an already-running dev instance instead of stacking windows.
    osascript -e "tell application id \"$(bundle_id)\" to quit" >/dev/null 2>&1 || true
    sleep 0.4
    open -n "$app"
    ;;

  test)
    shift
    profiled_cargo test --workspace "$@"
    ;;

  bench)
    shift
    cargo bench --workspace "$@"
    ;;

  lint)
    cargo fmt --all -- --check
    profiled_cargo clippy --workspace --all-targets -- -D warnings
    ;;

  fmt)
    cargo fmt --all
    ;;

  clean)
    cargo clean
    rm -rf target
    ;;

  *)
    sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
