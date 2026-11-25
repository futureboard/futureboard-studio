#!/usr/bin/env bash
# Bundle the xtask-staged runtime package into a macOS .app.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

PACKAGE_DIR="${1:-}"
OUT="${2:-$ROOT/packaging/native/out}"
APP_VERSION="${3:-}"

if [[ -z "$PACKAGE_DIR" ]]; then
  # A universal package (see merge-universal-macos.sh) is preferred over a
  # single-architecture one when both are present.
  for candidate in macos-universal macos-arm64 macos-x64; do
    if [[ -d "$ROOT/out/release/$candidate" ]]; then
      PACKAGE_DIR="$ROOT/out/release/$candidate"
      break
    fi
  done
  if [[ -z "$PACKAGE_DIR" ]]; then
    PACKAGE_DIR="$(find "$ROOT/out/release" -mindepth 1 -maxdepth 1 -type d -name 'macos-*' -print -quit 2>/dev/null || true)"
  fi
fi

APP_NAME="Futureboard Studio"
APP_DIR="$OUT/$APP_NAME.app"

# IMPORTANT:
# This must match CFBundleExecutable in Info.plist.
# If Info.plist says futureboard_native, keep this.
# If Info.plist says FutureboardNative, change this to FutureboardNative.
APP_EXECUTABLE_NAME="FutureboardNative"
CEF_HELPER_BINARY_NAME="futureboard_cef_helper"
CEF_HELPER_BASE_NAME="$APP_NAME Helper"
CEF_HELPER_VARIANTS=(
  "Helper"
  "Helper (GPU)"
  "Helper (Renderer)"
  "Helper (Plugin)"
  "Helper (Alerts)"
)

ICON_SRC="$ROOT/packages/shared/app/icons/icon.icns"
PLIST_SRC="$ROOT/packaging/native/Info.plist"
ENTITLEMENTS_SRC="$ROOT/packaging/native/Futureboard.entitlements"

CONTENTS="$APP_DIR/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"
FRAMEWORKS="$CONTENTS/Frameworks"

if [[ -z "$PACKAGE_DIR" || ! -f "$PACKAGE_DIR/FutureboardNative" ]]; then
  echo "error: xtask macOS runtime package not found: ${PACKAGE_DIR:-<none>}" >&2
  echo "run: cargo xtask package --profile release --plugin all" >&2
  exit 1
fi

if [[ ! -f "$PACKAGE_DIR/build-info.json" ]]; then
  echo "error: missing xtask package metadata: $PACKAGE_DIR/build-info.json" >&2
  exit 1
fi

if [[ -z "$APP_VERSION" ]]; then
  APP_VERSION="$(grep -oE '"version"[[:space:]]*:[[:space:]]*"[^"]+"' "$PACKAGE_DIR/build-info.json" | sed -E 's/.*"([^"]+)"/\1/' || true)"
fi
if [[ -z "$APP_VERSION" ]]; then
  echo "error: could not determine app version" >&2
  exit 1
fi
# Apple's bundle version fields accept numeric dot-separated components. The
# full channel-qualified SemVer remains embedded in the Rust binary/build-info.
BUNDLE_VERSION="${APP_VERSION%%[-+]*}"
if [[ ! "$BUNDLE_VERSION" =~ ^[0-9]+(\.[0-9]+)+$ ]]; then
  echo "error: invalid macOS bundle version: $BUNDLE_VERSION" >&2
  exit 1
fi

if [[ ! -f "$PLIST_SRC" ]]; then
  echo "error: Info.plist not found: $PLIST_SRC" >&2
  exit 1
fi
if [[ ! -f "$ENTITLEMENTS_SRC" ]]; then
  echo "error: entitlements not found: $ENTITLEMENTS_SRC" >&2
  exit 1
fi
if [[ ! -f "$PACKAGE_DIR/$CEF_HELPER_BINARY_NAME" ]]; then
  echo "error: staged macOS CEF helper not found: $PACKAGE_DIR/$CEF_HELPER_BINARY_NAME" >&2
  exit 1
fi

rm -rf "$APP_DIR"
mkdir -p "$MACOS" "$RESOURCES" "$FRAMEWORKS"

# Existing Info.plist
cp "$PLIST_SRC" "$CONTENTS/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $BUNDLE_VERSION" "$CONTENTS/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $BUNDLE_VERSION" "$CONTENTS/Info.plist"

# Preserve the validated xtask runtime layout, then place CEF in the standard
# macOS framework location expected by cef-rs' library loader.
cp -a "$PACKAGE_DIR/." "$MACOS/"
# Package metadata is a resource, not executable code. Keeping JSON inside
# Contents/MacOS makes codesign treat it as an unsigned nested code object.
mv "$MACOS/build-info.json" "$RESOURCES/build-info.json"
CEF_FRAMEWORK="$MACOS/Chromium Embedded Framework.framework"
if [[ ! -d "$CEF_FRAMEWORK" ]]; then
  echo "error: staged CEF framework not found: $CEF_FRAMEWORK" >&2
  exit 1
fi
mv "$CEF_FRAMEWORK" "$FRAMEWORKS/"
CEF_FRAMEWORK="$FRAMEWORKS/Chromium Embedded Framework.framework"
chmod +x "$MACOS/$APP_EXECUTABLE_NAME"
chmod +x "$MACOS/FutureboardPluginHostX64" "$MACOS/FutureboardPluginScanner"

# CEF 150's macOS sample packages five helper variants. They share the same
# minimal Rust executable but require distinct application/executable names so
# Chromium can select the appropriate role.
for VARIANT in "${CEF_HELPER_VARIANTS[@]}"; do
  HELPER_NAME="$APP_NAME $VARIANT"
  HELPER_APP="$FRAMEWORKS/$HELPER_NAME.app"
  HELPER_MACOS="$HELPER_APP/Contents/MacOS"
  mkdir -p "$HELPER_MACOS"
  cp "$MACOS/$CEF_HELPER_BINARY_NAME" "$HELPER_MACOS/$HELPER_NAME"
  chmod +x "$HELPER_MACOS/$HELPER_NAME"
  cat > "$HELPER_APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleExecutable</key><string>$HELPER_NAME</string>
  <key>CFBundleIdentifier</key><string>org.futureboard.studio.native</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>$HELPER_NAME</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$BUNDLE_VERSION</string>
  <key>CFBundleVersion</key><string>$BUNDLE_VERSION</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
  plutil -lint "$HELPER_APP/Contents/Info.plist" >/dev/null
done
rm "$MACOS/$CEF_HELPER_BINARY_NAME"

# Icon
if [[ -f "$ICON_SRC" ]]; then
  cp "$ICON_SRC" "$RESOURCES/icon.icns"
else
  echo "warning: missing $ICON_SRC — app will use default icon" >&2
fi

# Bind Info.plist (including the microphone purpose string) and the audio-input
# entitlement to a stable app-bundle identity. Developer ID distribution can
# replace this ad-hoc signature later; local/debug bundles still need a proper
# bundle signature so TCC grants microphone access to Futureboard Studio rather
# than to an unstable linker-generated executable identity.
#
# Nested Mach-O under Contents/MacOS (Plugins/*.dylib, onnxruntime, sidecar
# hosts) must be signed before the outer app. codesign treats those as nested
# code objects; leaving them unsigned fails the app signature with
# "code object is not signed at all". The main executable is signed with the
# bundle below.
while IFS= read -r -d '' BINARY; do
  [[ "$(basename "$BINARY")" == "$APP_EXECUTABLE_NAME" ]] && continue
  if lipo -archs "$BINARY" >/dev/null 2>&1; then
    codesign --force --sign - "$BINARY"
  fi
done < <(find "$MACOS" -type f -print0)

for VARIANT in "${CEF_HELPER_VARIANTS[@]}"; do
  HELPER_NAME="$APP_NAME $VARIANT"
  codesign --force --sign - "$FRAMEWORKS/$HELPER_NAME.app"
done
codesign --force --sign - "$CEF_FRAMEWORK"
codesign --force --sign - --entitlements "$ENTITLEMENTS_SRC" "$APP_DIR"
codesign --verify --deep --strict "$APP_DIR"

# Report the architectures actually shipped. Set FUTUREBOARD_REQUIRE_UNIVERSAL=1
# (release/nightly CI does) to make a single-architecture bundle a hard failure
# instead of a silent one nobody notices until an Intel Mac refuses to launch.
# Walk every file (not only +x): plugin dylibs are often mode 0644 after copy.
REQUIRE_UNIVERSAL="${FUTUREBOARD_REQUIRE_UNIVERSAL:-0}"
NOT_UNIVERSAL=()
while IFS= read -r -d '' BINARY; do
  ARCHS="$(lipo -archs "$BINARY" 2>/dev/null || true)"
  [[ -z "$ARCHS" ]] && continue
  echo "arch: ${BINARY#"$APP_DIR"/} -> $ARCHS"
  if [[ "$ARCHS" != *x86_64* || "$ARCHS" != *arm64* ]]; then
    NOT_UNIVERSAL+=("${BINARY#"$APP_DIR"/} [$ARCHS]")
  fi
done < <(find "$APP_DIR" -type f -print0)

if [[ ${#NOT_UNIVERSAL[@]} -gt 0 ]]; then
  if [[ "$REQUIRE_UNIVERSAL" == "1" ]]; then
    echo "error: FUTUREBOARD_REQUIRE_UNIVERSAL=1 but these are single-architecture:" >&2
    printf '  %s\n' "${NOT_UNIVERSAL[@]}" >&2
    exit 1
  fi
  echo "note: bundle is not universal (${#NOT_UNIVERSAL[@]} single-architecture binaries)"
fi

echo "Bundled macOS app: $APP_DIR"
echo
echo "Contents/MacOS:"
ls -la "$MACOS"
echo
echo "Contents/Frameworks:"
ls -la "$FRAMEWORKS"
echo
echo "Contents/Resources:"
ls -la "$RESOURCES"
