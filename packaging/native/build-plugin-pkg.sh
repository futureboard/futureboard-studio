#!/usr/bin/env bash
# Build the macOS installer package of a Futureboard plug-in (Rodhareist, WhiteSharp) and,
# with --upload, publish it to R2.
#
#   ./packaging/native/build-plugin-pkg.sh rodhareist|whitesharp [options]
#
# The macOS counterpart of packaging/windows/build-<plugin>-installer.ps1:
#
#   1. cargo <plugin>-vst3 bundle-universal <plugin>_vst3 --release, then
#      packaging/native/build-auv2.sh <plugin>                       (skip both: --skip-bundle)
#   2. pkgbuild one component package per format, productbuild them into
#      target/installer/<Name>_<ver>-macos.pkg with a choice per format
#   3. --upload: aws s3 cp -> r2://<bucket>/<plugin>-release/<Name>_<ver>-macos.pkg
#
# Formats and where they install: the Windows installer's three, plus the Audio Unit.
#
#   VST3  /Library/Audio/Plug-Ins/VST3/<Name>.vst3
#   CLAP  /Library/Audio/Plug-Ins/CLAP/<Name>.clap
#   VST2  /Library/Audio/Plug-Ins/VST/<Name>.vst
#   AU    /Library/Audio/Plug-Ins/Components/<Name>.component
#
# The version is read from vstplugins/<plugin>_vst3/Cargo.toml, the same value the plug-in
# reports to the host. The upload name is the contract with the website, which lists
# <plugin>-release/ and serves the highest <Name>_<version>-<platform>.<ext>.
#
# Signing: --sign-app "<Developer ID Application: …>" re-signs the bundles with the hardened
# runtime (the CLAP inside the AU first), --sign-installer "<Developer ID Installer: …>" signs
# the package. Without them the bundles keep their ad-hoc signatures and the package is
# unsigned, which Gatekeeper warns about on a downloaded copy. Neither is notarized here.
#
# R2 credentials come from the repository .env (CF_AWSS3_API, R2_ACCESS_KEY_ID,
# R2_SECRET_ACCESS_KEY), with real environment variables taking precedence, and are passed to
# the aws CLI through its environment only.
#
# Examples
#   ./packaging/native/build-plugin-pkg.sh rodhareist
#   ./packaging/native/build-plugin-pkg.sh whitesharp --skip-bundle --upload
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT_DIR="$ROOT/target/installer"
BUCKET="futureboard"
PREFIX=""
SKIP_BUNDLE=0
UPLOAD=0
SIGN_APP=""
SIGN_INSTALLER=""
APP_VERSION=""

die() {
  echo "error: $*" >&2
  exit 1
}
step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

PLUGIN="${1:-}"
case "$PLUGIN" in
  rodhareist) NAME="Rodhareist" ;;
  whitesharp) NAME="WhiteSharp" ;;
  -h|--help) sed -n '2,37p' "$0"; exit 0 ;;
  *) die "usage: $0 rodhareist|whitesharp [options] (see --help)" ;;
esac
shift
PLUGIN_DIR="$ROOT/vstplugins/${PLUGIN}_vst3"
BUNDLE_DIR="$PLUGIN_DIR/target/bundled"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-bundle) SKIP_BUNDLE=1; shift ;;
    --upload) UPLOAD=1; shift ;;
    --bucket) BUCKET="${2:?--bucket needs a value}"; shift 2 ;;
    --prefix) PREFIX="${2:?--prefix needs a value}"; shift 2 ;;
    --version) APP_VERSION="${2:?--version needs a value}"; shift 2 ;;
    --sign-app) SIGN_APP="${2:?--sign-app needs an identity}"; shift 2 ;;
    --sign-installer) SIGN_INSTALLER="${2:?--sign-installer needs an identity}"; shift 2 ;;
    -h|--help) sed -n '2,37p' "$0"; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

[[ "$(uname -s)" == "Darwin" ]] || die "this builds a macOS package; on Windows use packaging/windows/build-${PLUGIN}-installer.ps1"

[[ -n "$PREFIX" ]] || PREFIX="${PLUGIN}-release/"

if [[ -z "$APP_VERSION" ]]; then
  # The first `version =` is [package]'s; [workspace] has none.
  APP_VERSION="$(sed -nE 's/^version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' "$PLUGIN_DIR/Cargo.toml" | head -1)"
  [[ -n "$APP_VERSION" ]] || die "no package version found in $PLUGIN_DIR/Cargo.toml"
fi

OUTPUT_NAME="${NAME}_${APP_VERSION}-macos.pkg"
OUTPUT_PKG="$OUT_DIR/$OUTPUT_NAME"
echo "Version : $APP_VERSION"
echo "Output  : $OUTPUT_PKG"

# ── Bundles ─────────────────────────────────────────────────────────────────

if [[ "$SKIP_BUNDLE" -eq 0 ]]; then
  step "cargo ${PLUGIN}-vst3 bundle-universal ${PLUGIN}_vst3 --release"
  (cd "$ROOT" && cargo "${PLUGIN}-vst3" bundle-universal "${PLUGIN}_vst3" --release)
  "$ROOT/packaging/native/build-auv2.sh" "$PLUGIN"
fi

# format | bundle | install location | package identifier suffix
FORMATS=(
  "VST3|$NAME.vst3|/Library/Audio/Plug-Ins/VST3|vst3"
  "CLAP|$NAME.clap|/Library/Audio/Plug-Ins/CLAP|clap"
  "VST2|$NAME.vst|/Library/Audio/Plug-Ins/VST|vst2"
  "AU|$NAME.component|/Library/Audio/Plug-Ins/Components|auv2"
)

for entry in "${FORMATS[@]}"; do
  IFS='|' read -r label bundle _ _ <<<"$entry"
  binary="$BUNDLE_DIR/$bundle/Contents/MacOS/$NAME"
  [[ -f "$binary" ]] || die "$label bundle missing: $BUNDLE_DIR/$bundle (run without --skip-bundle)"
  archs="$(lipo -archs "$binary")"
  # A package for every Mac must not quietly ship a single-architecture binary.
  [[ "$archs" == *arm64* && "$archs" == *x86_64* ]] \
    || die "$bundle is not universal (has: $archs); build with bundle-universal"
done

# ── Package ─────────────────────────────────────────────────────────────────

WORK="$(mktemp -d "${TMPDIR:-/tmp}/${PLUGIN}-pkg.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/components" "$OUT_DIR"

CHOICES=""
CHOICE_LINES=""
for entry in "${FORMATS[@]}"; do
  IFS='|' read -r label bundle location suffix <<<"$entry"
  identifier="com.futureboard.$PLUGIN.$suffix"
  step "component $label -> $location"

  root="$WORK/root-$suffix"
  mkdir -p "$root"
  ditto "$BUNDLE_DIR/$bundle" "$root/$bundle"

  if [[ -n "$SIGN_APP" ]]; then
    # Inside out: the AU's embedded CLAP is sealed by the component's signature.
    for nested in "$root/$bundle"/Contents/PlugIns/*.clap; do
      [[ -e "$nested" ]] || continue
      codesign --force --timestamp --options runtime --sign "$SIGN_APP" "$nested"
    done
    codesign --force --timestamp --options runtime --sign "$SIGN_APP" "$root/$bundle"
  fi
  codesign --verify --strict "$root/$bundle"

  # Not relocatable: Installer would otherwise "update" any other copy of the bundle it can find
  # by identifier (a build tree, a backup) instead of installing to the location below.
  plist="$WORK/$suffix.plist"
  pkgbuild --analyze --root "$root" "$plist" >/dev/null
  plutil -replace 0.BundleIsRelocatable -bool NO "$plist"
  plutil -replace 0.BundleOverwriteAction -string upgrade "$plist"

  pkgbuild \
    --root "$root" \
    --component-plist "$plist" \
    --identifier "$identifier" \
    --version "$APP_VERSION" \
    --install-location "$location" \
    "$WORK/components/$suffix.pkg" >/dev/null

  CHOICES+="        <line choice=\"$suffix\"/>"$'\n'
  CHOICE_LINES+="    <choice id=\"$suffix\" visible=\"true\" start_selected=\"true\" title=\"$label\" description=\"$NAME $label plug-in, installed to $location.\">
        <pkg-ref id=\"$identifier\"/>
    </choice>
    <pkg-ref id=\"$identifier\" version=\"$APP_VERSION\" onConclusion=\"none\">$suffix.pkg</pkg-ref>
"
done

cat >"$WORK/distribution.xml" <<EOF
<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>$NAME $APP_VERSION</title>
    <options customize="allow" require-scripts="false" hostArchitectures="arm64,x86_64"/>
    <domains enable_anywhere="false" enable_currentUserHome="false" enable_localSystem="true"/>
    <choices-outline>
$CHOICES    </choices-outline>
$CHOICE_LINES</installer-gui-script>
EOF

step "productbuild $OUTPUT_NAME"
rm -f "$OUTPUT_PKG"
sign_args=()
[[ -n "$SIGN_INSTALLER" ]] && sign_args=(--sign "$SIGN_INSTALLER" --timestamp)
productbuild \
  --distribution "$WORK/distribution.xml" \
  --package-path "$WORK/components" \
  ${sign_args[@]+"${sign_args[@]}"} \
  "$OUTPUT_PKG"

size_mb="$(du -m "$OUTPUT_PKG" | cut -f1)"
echo "Installer: $OUTPUT_PKG (${size_mb} MB)"
[[ -z "$SIGN_INSTALLER" ]] && echo "note: the package is unsigned (pass --sign-installer to sign it)"

[[ "$UPLOAD" -eq 1 ]] || exit 0

# ── Upload to R2 ────────────────────────────────────────────────────────────

command -v aws >/dev/null 2>&1 || die "the aws CLI is required for --upload (brew install awscli)"

config() {
  local name="$1" value="${!1:-}"
  if [[ -z "$value" && -f "$ROOT/.env" ]]; then
    value="$(sed -nE "s/^[[:space:]]*$name[[:space:]]*=[[:space:]]*//p" "$ROOT/.env" | tail -1)"
    value="${value%\"}"; value="${value#\"}"; value="${value%\'}"; value="${value#\'}"
  fi
  printf '%s' "$value"
}

ENDPOINT="$(config CF_AWSS3_API)"
KEY_ID="$(config R2_ACCESS_KEY_ID)"
SECRET="$(config R2_SECRET_ACCESS_KEY)"
for pair in "CF_AWSS3_API:$ENDPOINT" "R2_ACCESS_KEY_ID:$KEY_ID" "R2_SECRET_ACCESS_KEY:$SECRET"; do
  [[ -n "${pair#*:}" ]] || die "${pair%%:*} is missing from the environment and $ROOT/.env"
done

TARGET="s3://$BUCKET/$PREFIX$OUTPUT_NAME"
step "upload -> $TARGET"
AWS_ACCESS_KEY_ID="$KEY_ID" AWS_SECRET_ACCESS_KEY="$SECRET" AWS_DEFAULT_REGION=auto \
  aws s3 cp "$OUTPUT_PKG" "$TARGET" \
    --endpoint-url "$ENDPOINT" \
    --content-type "application/octet-stream" \
    --only-show-errors
echo "Uploaded: $TARGET"
