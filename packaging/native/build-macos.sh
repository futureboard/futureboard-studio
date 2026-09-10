#!/usr/bin/env bash
# Build and package Futureboard Studio for macOS, end to end.
#
#   build-macos.sh [--arch all|arm64|x86_64|universal] [options]
#
# This is the local driver for the sequence CI runs (.github/workflows —
# "Package native runtime with xtask (macOS universal)" onwards). Nothing here
# is a second implementation of it: every step below calls the same xtask
# command or the same script the workflow does, in the same order.
#
#   xtask package --target <triple>   one runtime tree per architecture
#     └─ merge-universal-macos.sh     lipo the two trees into one
#          └─ bundle-macos.sh         the .app
#               └─ bundle-macos-dmg.sh   the image
#
# CEF is why there are two passes rather than one. Chromium publishes macosx64
# and macosarm64 separately and no universal distribution, so a universal
# Futureboard is two full builds joined with `lipo`. That is also why
# `--arch universal` needs both CEF trees staged locally, and why this script
# says so up front instead of failing an hour into the second build.
#
# Options
#   --arch <a>       all (default) | arm64 | x86_64 | universal
#                    `all` builds both slices, then the universal merge, and
#                    bundles all three — what a release produces.
#   --profile <p>    cargo profile (default: release)
#   --edition <e>    community (default) — see xtask --edition
#   --plugin <spec>  all (default) | none | comma-separated crate names
#   --version <v>    version string for the bundle (default: version.json)
#   --out <dir>      where .app/.dmg go (default: packaging/native/out)
#   --no-dmg         stop after the .app
#   --no-build       package/bundle what is already staged in out/
#   --clean          remove the staged trees and bundles for this run first
#   -h, --help       this text
#
# Examples
#   ./packaging/native/build-macos.sh --arch arm64 --no-dmg   # fastest local loop
#   ./packaging/native/build-macos.sh                          # what CI ships
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

ARCH="all"
PROFILE="release"
EDITION="community"
PLUGIN="all"
APP_VERSION=""
OUT_DIR="$ROOT/packaging/native/out"
WANT_DMG=1
WANT_BUILD=1
WANT_CLEAN=0

die() {
  echo "error: $*" >&2
  exit 1
}
step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
note() { printf '    %s\n' "$*"; }

# The header comment above is the help text. Printed by walking it rather than
# by a line range, so editing the header cannot silently truncate `--help`.
usage() {
  awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "$0"
  exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --arch) ARCH="${2:?--arch needs a value}"; shift 2 ;;
    --profile) PROFILE="${2:?--profile needs a value}"; shift 2 ;;
    --edition) EDITION="${2:?--edition needs a value}"; shift 2 ;;
    --plugin|--plugins) PLUGIN="${2:?--plugin needs a value}"; shift 2 ;;
    --version) APP_VERSION="${2:?--version needs a value}"; shift 2 ;;
    --out) OUT_DIR="${2:?--out needs a value}"; shift 2 ;;
    --no-dmg) WANT_DMG=0; shift ;;
    --no-build) WANT_BUILD=0; shift ;;
    --clean) WANT_CLEAN=1; shift ;;
    -h|--help) usage 0 ;;
    *) echo "error: unknown option: $1" >&2; usage 1 ;;
  esac
done

case "$ARCH" in
  all|arm64|x86_64|universal) ;;
  x64) ARCH="x86_64" ;;
  aarch64) ARCH="arm64" ;;
  *) die "--arch must be all, arm64, x86_64 or universal (got: $ARCH)" ;;
esac

[[ "$(uname -s)" == "Darwin" ]] || die "this builds macOS bundles and needs macOS tooling (lipo, hdiutil, codesign)"

# ── What each architecture is called, in the three vocabularies involved ─────
#
# The Rust triple, the folder xtask stages into (`crate::platform::
# platform_folder`), and the CEF distribution directory. Keeping the mapping in
# one place is what stops a build packaging one architecture's binaries beside
# another's CEF.
triple_for() {
  case "$1" in
    arm64) echo "aarch64-apple-darwin" ;;
    x86_64) echo "x86_64-apple-darwin" ;;
  esac
}
package_folder_for() {
  case "$1" in
    arm64) echo "macos-arm64" ;;
    x86_64) echo "macos-x64" ;;
    universal) echo "macos-universal" ;;
  esac
}
cef_dir_for() {
  case "$1" in
    arm64) echo "cef_macos_aarch64" ;;
    x86_64) echo "cef_macos_x86_64" ;;
  esac
}

STAGE_ROOT="$ROOT/out/$PROFILE/$EDITION"
package_dir_for() { echo "$STAGE_ROOT/$(package_folder_for "$1")"; }

# Which slices this run has to compile. `universal` is a merge of the two, so it
# needs both even though it is one output.
SLICES=()
case "$ARCH" in
  arm64) SLICES=(arm64) ;;
  x86_64) SLICES=(x86_64) ;;
  universal|all) SLICES=(x86_64 arm64) ;;
esac

# Which bundles come out at the end.
VARIANTS=()
case "$ARCH" in
  arm64) VARIANTS=(arm64) ;;
  x86_64) VARIANTS=(x86_64) ;;
  universal) VARIANTS=(universal) ;;
  all) VARIANTS=(arm64 x86_64 universal) ;;
esac

# ── Preflight ───────────────────────────────────────────────────────────────
#
# Everything checked here is something that otherwise fails deep inside a build
# that has already run for a long time.

step "Preflight"

command -v cargo >/dev/null || die "cargo not found on PATH"

# The CEF version the app is compiled against, read from the source rather than
# repeated here: `crates/SphereWebView` is the only place that gets to decide
# it, and a copy would drift the first time it moves.
CEF_VERSION="$(sed -nE 's/.*CEF_SHORT_VERSION: &str = "([^"]+)".*/\1/p' \
  "$ROOT/crates/SphereWebView/src/lib.rs" | head -n1)"
[[ -n "$CEF_VERSION" ]] || die "could not read CEF_SHORT_VERSION from crates/SphereWebView/src/lib.rs"
note "CEF $CEF_VERSION"

if [[ "$WANT_BUILD" -eq 1 ]]; then
  installed_targets="$(rustup target list --installed 2>/dev/null || true)"
  # A newline-joined list rather than an array: macOS ships bash 3.2, where
  # expanding an empty array under `set -u` is an unbound-variable error, and a
  # preflight that crashes when everything is fine is worse than no preflight.
  missing_cef=""
  for slice in "${SLICES[@]}"; do
    triple="$(triple_for "$slice")"

    if [[ -n "$installed_targets" ]] && ! grep -qx "$triple" <<<"$installed_targets"; then
      die "rust target $triple is not installed — run: rustup target add $triple"
    fi

    cef_path="$ROOT/build/cef/$CEF_VERSION/$(cef_dir_for "$slice")"
    if [[ ! -d "$cef_path" ]]; then
      missing_cef="${missing_cef}  ${slice}: ${cef_path}"$'\n'
    fi
  done

  if [[ -n "$missing_cef" ]]; then
    echo "error: the CEF distribution for one or more slices is not staged:" >&2
    printf '%s' "$missing_cef" >&2
    cat >&2 <<'EOF'

Chromium publishes one distribution per architecture and no universal one, so
each slice needs its own tree under build/cef/<version>/. Stage the missing
one(s) there, or build only the architecture you have:

  ./packaging/native/build-macos.sh --arch arm64
EOF
    exit 1
  fi
  note "CEF distributions present for: ${SLICES[*]}"
fi

if [[ -z "$APP_VERSION" ]]; then
  APP_VERSION="$(sed -nE 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' \
    "$ROOT/version.json" | head -n1)"
fi
[[ -n "$APP_VERSION" ]] || die "could not determine app version — pass --version"
note "version $APP_VERSION"
note "profile $PROFILE, edition $EDITION, plugins $PLUGIN"
note "bundling: ${VARIANTS[*]}"

if [[ "$WANT_CLEAN" -eq 1 ]]; then
  step "Clean"
  # Written as `if` rather than `[[ … ]] && { … }`: under `set -e` the value of
  # an and-list that never ran is a question nobody should have to answer while
  # reading a build script.
  for slice in "${SLICES[@]}" universal; do
    dir="$(package_dir_for "$slice")"
    if [[ -d "$dir" ]]; then
      note "rm $dir"
      rm -rf "$dir"
    fi
  done
  for variant in "${VARIANTS[@]}"; do
    dir="$OUT_DIR/$variant"
    if [[ -d "$dir" ]]; then
      note "rm $dir"
      rm -rf "$dir"
    fi
  done
fi

# ── Build + stage, one pass per architecture ────────────────────────────────
#
# The deployment target is set for the whole run rather than per pass: a
# universal binary whose two slices disagree about their minimum OS is a binary
# that runs on different machines depending on which slice is picked.
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"

if [[ "$WANT_BUILD" -eq 1 ]]; then
  for slice in "${SLICES[@]}"; do
    triple="$(triple_for "$slice")"
    step "Package $slice ($triple)"
    # xtask hands Cargo the CEF distribution matching `--target`, so no
    # per-pass CEF_PATH is set here — same as the workflow.
    cargo run -p xtask -- package \
      --profile "$PROFILE" \
      --edition "$EDITION" \
      --plugin "$PLUGIN" \
      --target "$triple"
  done
else
  step "Skipping build (--no-build)"
fi

for slice in "${SLICES[@]}"; do
  dir="$(package_dir_for "$slice")"
  [[ -f "$dir/FutureboardNative" && -f "$dir/build-info.json" ]] \
    || die "$dir is not a complete xtask package — drop --no-build, or check the build above"
done

if [[ " ${VARIANTS[*]} " == *" universal "* ]]; then
  step "Merge universal"
  bash "$ROOT/packaging/native/merge-universal-macos.sh" \
    "$(package_dir_for x86_64)" \
    "$(package_dir_for arm64)" \
    "$(package_dir_for universal)"
fi

# ── Bundle ──────────────────────────────────────────────────────────────────
#
# One .app (and image) per variant, each from its own staged tree. The
# single-architecture bundles come first so a universal-only codesign problem
# cannot hold back the Apple Silicon and Intel images — the same order, and the
# same reason, as the release workflow.
for variant in "${VARIANTS[@]}"; do
  package_dir="$(package_dir_for "$variant")"
  app_out="$OUT_DIR/$variant"
  app="$app_out/Futureboard Studio.app"

  step "Bundle $variant"
  # Only the universal bundle is required to be fat; the others are one slice
  # by definition, and demanding otherwise would fail every local single-arch
  # build.
  require_universal=0
  if [[ "$variant" == "universal" ]]; then
    require_universal=1
  fi
  FUTUREBOARD_REQUIRE_UNIVERSAL="$require_universal" \
    bash "$ROOT/packaging/native/bundle-macos.sh" "$package_dir" "$app_out" "$APP_VERSION"

  if [[ "$WANT_DMG" -eq 1 ]]; then
    step "Image $variant"
    # The image is named after the architectures the binary actually carries,
    # not after `$variant` — see bundle-macos-dmg.sh.
    bash "$ROOT/packaging/native/bundle-macos-dmg.sh" "$app" "$OUT_DIR" "$APP_VERSION"
  fi
done

step "Done"
for variant in "${VARIANTS[@]}"; do
  note "$OUT_DIR/$variant/Futureboard Studio.app"
done
if [[ "$WANT_DMG" -eq 1 ]]; then
  # Listed rather than constructed: an image is named after the architectures
  # its binary actually carries, which only bundle-macos-dmg.sh knows.
  for image in "$OUT_DIR"/*.dmg; do
    [[ -e "$image" ]] || break
    note "$image"
  done
fi
