#!/usr/bin/env bash
# Build the Audio Unit (AUv2) of a Futureboard plug-in from its CLAP build.
#
#   ./packaging/native/build-auv2.sh rodhareist [--bundle]
#   ./packaging/native/build-auv2.sh whitesharp [--bundle]
#
# nih-plug has no Audio Unit wrapper, so the AU is clap-wrapper's AUv2 shell
# (packaging/native/auv2/CMakeLists.txt) around the plug-in's own CLAP, which is
# copied into the component at Contents/PlugIns/<Name>.clap. The AU therefore
# runs the same binary as the CLAP: same DSP, parameters, state and editor.
#
# Input   vstplugins/<plugin>_vst3/target/bundled/<Name>.clap
#         (--bundle runs `cargo <plugin>-vst3 bundle-universal <plugin>_vst3 --release` first)
# Output  vstplugins/<plugin>_vst3/target/bundled/<Name>.component, next to the
#         VST3/CLAP/VST2 bundles, ad-hoc signed.
#
# The AU is an effect ('aufx'), manufacturer 'Ftbd', with a fixed subtype per
# plug-in: hosts identify an AU by type/subtype/manufacturer, so never change them.
#
# Needs cmake and ninja (brew install cmake ninja) and the submodules
# external/clap, external/clap-wrapper and external/AudioUnitSDK.
# Check the result with: auval -v aufx <subtype> Ftbd (after installing it).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

die() {
  echo "error: $*" >&2
  exit 1
}
step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

PLUGIN="${1:-}"
shift || true
BUNDLE=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --bundle) BUNDLE=1; shift ;;
    -h|--help) sed -n '2,23p' "$0"; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

# plugin | bundle name | AU subtype | identifier base
case "$PLUGIN" in
  rodhareist) NAME="Rodhareist"; SUBTYPE="Rdhr"; IDENT="com.futureboard.rodhareist" ;;
  whitesharp) NAME="WhiteSharp"; SUBTYPE="WShp"; IDENT="com.futureboard.whitesharp" ;;
  -h|--help) sed -n '2,23p' "$0"; exit 0 ;;
  *) die "usage: $0 rodhareist|whitesharp [--bundle]" ;;
esac

[[ "$(uname -s)" == "Darwin" ]] || die "an Audio Unit is built on macOS only"
command -v cmake >/dev/null 2>&1 || die "cmake is required (brew install cmake)"
command -v ninja >/dev/null 2>&1 || die "ninja is required (brew install ninja)"
for sdk in clap clap-wrapper AudioUnitSDK; do
  [[ -n "$(ls -A "$ROOT/external/$sdk" 2>/dev/null)" ]] \
    || die "external/$sdk is empty; run: git submodule update --init external/$sdk"
done

PLUGIN_DIR="$ROOT/vstplugins/${PLUGIN}_vst3"
BUNDLE_DIR="$PLUGIN_DIR/target/bundled"
CLAP="$BUNDLE_DIR/$NAME.clap"
BUILD_DIR="$ROOT/target/auv2/$PLUGIN"

# The first `version =` is [package]'s. An AU version is numbers only, so the
# component carries the part before any pre-release tag: 2026.11.1-beta2.0.0 -> 2026.11.1.
FULL_VERSION="$(sed -nE 's/^version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' "$PLUGIN_DIR/Cargo.toml" | head -1)"
[[ -n "$FULL_VERSION" ]] || die "no package version found in $PLUGIN_DIR/Cargo.toml"
VERSION="$(printf '%s' "$FULL_VERSION" | sed -E 's/[^0-9.].*$//; s/\.$//')"
[[ -n "$VERSION" ]] || die "version '$FULL_VERSION' does not start with a number"

if [[ "$BUNDLE" -eq 1 ]]; then
  step "cargo ${PLUGIN}-vst3 bundle-universal ${PLUGIN}_vst3 --release"
  (cd "$ROOT" && cargo "${PLUGIN}-vst3" bundle-universal "${PLUGIN}_vst3" --release)
fi
[[ -f "$CLAP/Contents/MacOS/$NAME" ]] || die "no CLAP bundle at $CLAP (run with --bundle)"

step "$NAME.component $VERSION (aufx/$SUBTYPE/Ftbd)"
# A fresh tree every time: clap-wrapper copies the CLAP into the component only
# when the wrapper relinks, so an incremental build would keep a stale CLAP.
rm -rf "$BUILD_DIR"
cmake -S "$ROOT/packaging/native/auv2" -B "$BUILD_DIR" -G Ninja \
  -DCMAKE_BUILD_TYPE=Release \
  -DFB_AU_NAME="$NAME" \
  -DFB_AU_CLAP="$CLAP" \
  -DFB_AU_SUBTYPE="$SUBTYPE" \
  -DFB_AU_VERSION="$VERSION" \
  -DFB_AU_BUNDLE_ID="$IDENT" \
  -Wno-dev >/dev/null
cmake --build "$BUILD_DIR"

COMPONENT="$BUILD_DIR/$NAME.component"
[[ -f "$COMPONENT/Contents/MacOS/$NAME" ]] || die "the build produced no $COMPONENT"
[[ -f "$COMPONENT/Contents/PlugIns/$NAME.clap/Contents/MacOS/$NAME" ]] \
  || die "$COMPONENT does not carry $NAME.clap"

# Ad-hoc: the embedded CLAP keeps the bundler's signature, the component seals it.
codesign --force --sign - "$COMPONENT"
codesign --verify --strict "$COMPONENT"

rm -rf "$BUNDLE_DIR/$NAME.component"
ditto "$COMPONENT" "$BUNDLE_DIR/$NAME.component"
echo "AU: $BUNDLE_DIR/$NAME.component ($(lipo -archs "$BUNDLE_DIR/$NAME.component/Contents/MacOS/$NAME"))"
