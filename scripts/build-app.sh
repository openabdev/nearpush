#!/bin/sh
# Builds build/Nearpush.app (menu bar app + bundled nearpushd) and build/nearpush.
# Runs on the build host, never on the laptop:
#   ssh <build-host> 'cd ~/repo/nearpush && export PATH=$HOME/.cargo/bin:$PATH && scripts/build-app.sh'
# Signing: ad-hoc by default; SIGN_ID="Apple Development: …" to use an identity.
set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)

# NEARPUSH_BUILD_HOST is a shell glob matching the build host's name, e.g. "build-*".
host=$(scutil --get LocalHostName 2>/dev/null || uname -n)
case "$host" in
  ${NEARPUSH_BUILD_HOST:-__unset__}) ;;
  *)
    if [ -z "${NEARPUSH_ALLOW_LOCAL_BUILD:-}" ]; then
      echo "build-app.sh: refusing to build on $host; run it on the build host (set NEARPUSH_BUILD_HOST to its name glob) or set NEARPUSH_ALLOW_LOCAL_BUILD=1" >&2
      exit 2
    fi
    ;;
esac

VERSION=$(awk '
  /^\[workspace\.package\]/ { inpkg = 1; next }
  /^\[/ { inpkg = 0 }
  inpkg && /^version[[:space:]]*=/ { gsub(/.*=[[:space:]]*"|".*/, ""); print; exit }
' Cargo.toml)
[ -n "$VERSION" ] || { echo "build-app.sh: cannot read [workspace.package] version" >&2; exit 1; }
SIGN="${SIGN_ID:--}"

echo "==> cargo build --release (nearpushd, nearpush) $VERSION"
cargo build --release -p nearpushd -p nearpush

echo "==> swift build -c release (Nearpush)"
(cd macos && swift build -c release --arch arm64)
SWIFT_BIN=$(cd macos && swift build -c release --arch arm64 --show-bin-path)

APP="$ROOT/build/Nearpush.app"
echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Library/LaunchAgents" "$APP/Contents/Resources"
cp "$SWIFT_BIN/Nearpush" "$APP/Contents/MacOS/Nearpush"
cp target/release/nearpushd "$APP/Contents/MacOS/nearpushd"
sed "s/@VERSION@/$VERSION/g" macos/Resources/Info.plist > "$APP/Contents/Info.plist"
cp macos/Resources/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
cp macos/Resources/dev.nearpush.agent.plist "$APP/Contents/Library/LaunchAgents/dev.nearpush.agent.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"

plutil -lint "$APP/Contents/Info.plist" "$APP/Contents/Library/LaunchAgents/dev.nearpush.agent.plist"
for key in CFBundleShortVersionString CFBundleVersion; do
  [ "$(plutil -extract "$key" raw "$APP/Contents/Info.plist")" = "$VERSION" ] || { echo "build-app.sh: $key in Info.plist is not $VERSION" >&2; exit 1; }
done

echo "==> codesign ($SIGN)"
# Inner code first, then the bundle; no --deep.
codesign --force --sign "$SIGN" --timestamp=none --identifier dev.nearpush.nearpushd "$APP/Contents/MacOS/nearpushd"
codesign --force --sign "$SIGN" --timestamp=none "$APP"
codesign --verify --strict --verbose=2 "$APP"

cp target/release/nearpush "$ROOT/build/nearpush"
ZIP="$ROOT/build/Nearpush-$VERSION-macos-arm64.zip"
rm -f "$ZIP"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"

echo
echo "app:  $APP"
echo "zip:  $ZIP"
echo "cli:  $ROOT/build/nearpush"
