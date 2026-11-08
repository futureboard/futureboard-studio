#!/usr/bin/env bash
# Bundle the Community Edition FutureboardNative into a portable Linux AppImage.
#
# Usage: bundle-appimage.sh [PACKAGE_DIR] [OUT_DIR] [APP_VERSION]
#   PACKAGE_DIR Complete runtime tree produced by `xtask package`.
#               Default: out/release/community/linux-x64
#   OUT_DIR     Directory the finished .AppImage is written to.
#               Default: target/appimage
#   APP_VERSION Version string embedded in the AppImage filename.
#               Default: read from version.json
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

PACKAGE_DIR="${1:-$ROOT/out/release/community/linux-x64}"
OUT_DIR="${2:-$ROOT/target/appimage}"
APP_VERSION="${3:-}"
BIN="$PACKAGE_DIR/FutureboardNative"

if [[ -z "$APP_VERSION" ]]; then
  # Avoid a hard `node` dependency for a one-line JSON read.
  APP_VERSION="$(grep -oP '"version"\s*:\s*"\K[^"]+' "$ROOT/version.json" 2>/dev/null || true)"
fi
if [[ -z "$APP_VERSION" ]]; then
  echo "error: could not determine app version (pass it as \$3, or ensure version.json is readable)" >&2
  exit 1
fi

DESKTOP_SLUG="futureboard-studio"
PKG_DIR="$ROOT/packaging/linux"
DESKTOP_SRC="$PKG_DIR/futureboard-studio.desktop"
APPRUN_SRC="$PKG_DIR/AppRun"
MIME_SRC="$PKG_DIR/futureboard-studio-mime.xml"
ICON_SRC="$ROOT/packages/shared/app/icons/app.png"

APPDIR="$OUT_DIR/AppDir"

if [[ ! -f "$BIN" ]]; then
  echo "error: xtask runtime package not found: $BIN" >&2
  echo "run: cargo xtask package --profile release --edition community --plugin all" >&2
  exit 1
fi
if [[ ! -f "$PACKAGE_DIR/build-info.json" ]]; then
  echo "error: missing xtask package metadata: $PACKAGE_DIR/build-info.json" >&2
  exit 1
fi
for f in "$DESKTOP_SRC" "$APPRUN_SRC" "$MIME_SRC" "$ICON_SRC"; do
  test -f "$f" || { echo "error: missing packaging asset: $f" >&2; exit 1; }
done

rm -rf "$APPDIR"
mkdir -p \
  "$APPDIR/usr/bin" \
  "$APPDIR/usr/lib" \
  "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/512x512/apps" \
  "$APPDIR/usr/share/mime/packages"

# Preserve the validated xtask layout as one unit. CEF resources/locales,
# helper processes, built-in plugins, runtime libraries and build-info.json
# must remain beside the main executable exactly as xtask staged them.
cp -a "$PACKAGE_DIR/." "$APPDIR/usr/bin/"
chmod +x \
  "$APPDIR/usr/bin/FutureboardNative" \
  "$APPDIR/usr/bin/FutureboardPluginHostX64" \
  "$APPDIR/usr/bin/FutureboardPluginScanner"

install -m755 "$APPRUN_SRC" "$APPDIR/AppRun"

install -m644 "$DESKTOP_SRC" "$APPDIR/${DESKTOP_SLUG}.desktop"
install -m644 "$DESKTOP_SRC" "$APPDIR/usr/share/applications/${DESKTOP_SLUG}.desktop"

install -m644 "$ICON_SRC" "$APPDIR/${DESKTOP_SLUG}.png"
install -m644 "$ICON_SRC" "$APPDIR/usr/share/icons/hicolor/512x512/apps/${DESKTOP_SLUG}.png"

install -m644 "$MIME_SRC" "$APPDIR/usr/share/mime/packages/${DESKTOP_SLUG}.xml"

# appimagetool and the AppImage runtime it embeds as the image's ELF header.
# Both are pinned to a versioned release (never the moving `continuous` tag)
# and checked against a SHA-256 before use, cached copies included: `target/`
# is restored from the CI cache, so a cached file is trusted no more than a
# fresh download. Without `--runtime-file` appimagetool would fetch the
# runtime from `continuous` itself at build time and ship it unverified.
# The hashes are the digests GitHub records for these release assets; bump a
# version and its hash together.
APPIMAGETOOL_VERSION="1.9.1"
APPIMAGETOOL_SHA256="ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0"
APPIMAGETOOL_URL="https://github.com/AppImage/appimagetool/releases/download/${APPIMAGETOOL_VERSION}/appimagetool-x86_64.AppImage"
APPIMAGE_RUNTIME_VERSION="20251108"
APPIMAGE_RUNTIME_SHA256="2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d"
APPIMAGE_RUNTIME_URL="https://github.com/AppImage/type2-runtime/releases/download/${APPIMAGE_RUNTIME_VERSION}/runtime-x86_64"

sha256_of() {
  sha256sum "$1" | cut -d' ' -f1
}

# fetch_verified URL SHA256 DEST: leave DEST holding exactly the pinned bytes,
# or fail without leaving anything at DEST.
fetch_verified() {
  local url="$1" expected="$2" dest="$3"
  if [[ -f "$dest" && "$(sha256_of "$dest")" == "$expected" ]]; then
    return 0
  fi
  rm -f "$dest"
  echo "Fetching $url"
  local tmp="$dest.download.$$"
  rm -f "$tmp"
  curl -fsSL --retry 3 -o "$tmp" "$url"
  local actual
  actual="$(sha256_of "$tmp")"
  if [[ "$actual" != "$expected" ]]; then
    rm -f "$tmp"
    echo "error: SHA-256 mismatch for $url" >&2
    echo "  expected $expected" >&2
    echo "  actual   $actual" >&2
    exit 1
  fi
  mv -f "$tmp" "$dest"
}

mkdir -p "$ROOT/target"
APPIMAGETOOL="$ROOT/target/appimagetool-${APPIMAGETOOL_VERSION}-x86_64.AppImage"
APPIMAGE_RUNTIME="$ROOT/target/appimage-runtime-${APPIMAGE_RUNTIME_VERSION}-x86_64"
fetch_verified "$APPIMAGETOOL_URL" "$APPIMAGETOOL_SHA256" "$APPIMAGETOOL"
fetch_verified "$APPIMAGE_RUNTIME_URL" "$APPIMAGE_RUNTIME_SHA256" "$APPIMAGE_RUNTIME"
chmod +x "$APPIMAGETOOL"

mkdir -p "$OUT_DIR"
OUTPUT="$OUT_DIR/Futureboard.Studio-${APP_VERSION}-x86_64.AppImage"
rm -f "$OUTPUT"

# `--appimage-extract-and-run` makes appimagetool run itself without FUSE,
# which most CI containers don't have `/dev/fuse` for.
ARCH=x86_64 "$APPIMAGETOOL" --appimage-extract-and-run \
  --runtime-file "$APPIMAGE_RUNTIME" "$APPDIR" "$OUTPUT"

echo "Built AppImage: $OUTPUT"
echo
echo "AppImage runtime binaries:"
ls -la "$APPDIR/usr/bin"
echo
echo "AppImage runtime libraries:"
ls -la "$APPDIR/usr/lib"
