#!/usr/bin/env bash
# Merge two xtask macOS runtime packages into one universal (fat) package.
#
#   merge-universal-macos.sh <x86_64-package-dir> <arm64-package-dir> [out-dir]
#
# CEF publishes macosx64 and macosarm64 separately — there is no universal
# distribution — so a universal Futureboard build is produced by packaging each
# architecture with `cargo run -p xtask -- package --target <triple>` and
# joining the two trees here:
#
#   * Mach-O files (app, sidecars, CEF framework, plugin dylibs) are `lipo`d
#     into one binary carrying both slices: the x86_64 slice from the x86_64
#     package and the arm64 slice from the arm64 package. An input that is
#     already fat (ONNX Runtime downloads as universal2) is thinned first.
#   * Files whose name carries an architecture — Chromium's
#     `v8_context_snapshot.arm64.bin` / `.x86_64.bin` — are taken from whichever
#     package has them. A universal framework is expected to hold both, and the
#     running slice picks its own at startup.
#   * Everything else (.pak, icudtl.dat, JSON) is architecture-independent and
#     is copied from the arm64 tree.
#
# Anything else present in only one package is an error: it would mean shipping
# a universal app that is silently incomplete for one architecture.
#
# The result is the same layout bundle-macos.sh already consumes.
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: this script needs macOS tooling (lipo)" >&2
  exit 1
fi

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

X64_DIR="${1:-$ROOT/out/release/community/macos-x64}"
ARM64_DIR="${2:-$ROOT/out/release/community/macos-arm64}"
OUT_DIR="${3:-$ROOT/out/release/community/macos-universal}"

# Must match crate::platform::platform_folder / the triple xtask records.
UNIVERSAL_PLATFORM="macos-universal"
UNIVERSAL_TARGET="universal-apple-darwin"

for dir in "$X64_DIR" "$ARM64_DIR"; do
  if [[ ! -d "$dir" ]]; then
    echo "error: package directory not found: $dir" >&2
    echo "run: cargo run -p xtask -- package --profile release --edition community \\" >&2
    echo "       --plugin all --target x86_64-apple-darwin   (and aarch64-apple-darwin)" >&2
    exit 1
  fi
  if [[ ! -f "$dir/FutureboardNative" || ! -f "$dir/build-info.json" ]]; then
    echo "error: $dir is not a complete xtask macOS package" >&2
    exit 1
  fi
done

if [[ "$(cd "$X64_DIR" && pwd)" == "$(cd "$ARM64_DIR" && pwd)" ]]; then
  echo "error: both inputs resolve to the same directory" >&2
  exit 1
fi

is_macho() {
  lipo -archs "$1" >/dev/null 2>&1
}

# Write the `$2` slice of Mach-O `$1` to `$3`. A single-architecture input is
# copied; a fat one (ONNX Runtime ships as universal2 in *both* packages) is
# thinned, so the merge never hands lipo two files that both carry a slice —
# which it refuses. Fails when the file has no such slice at all.
extract_slice() {
  local file="$1" arch="$2" out="$3" archs
  archs="$(lipo -archs "$file")"
  if [[ "$archs" == "$arch" ]]; then
    cp "$file" "$out"
  elif [[ " $archs " == *" $arch "* ]]; then
    lipo "$file" -thin "$arch" -output "$out"
  else
    echo "error: ${file} has no $arch slice (has: $archs)" >&2
    return 1
  fi
}

SLICE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fb-universal-slices.XXXXXX")"
trap 'rm -rf "$SLICE_DIR"' EXIT

# Chromium names its per-architecture resources `<name>.<arch>.<ext>`, so a
# single-architecture package legitimately holds only its own. These are
# unioned into the universal package rather than required on both sides.
is_arch_specific() {
  case "${1##*/}" in
    *.arm64.* | *.x86_64.* | *-arm64.* | *-x86_64.* | *_arm64.* | *_x86_64.*) return 0 ;;
    *) return 1 ;;
  esac
}

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

merged=0
copied=0
differing=()
arch_only=()

# Mirror the directory tree first: xtask creates layout directories such as
# `Resources/` that can legitimately be empty, and a file-only walk would drop
# them from the universal package.
while IFS= read -r -d '' directory; do
  mkdir -p "$OUT_DIR/${directory#"$ARM64_DIR"/}"
done < <(find "$ARM64_DIR" -mindepth 1 -type d -print0)

# The arm64 tree is the reference layout; anything it contains must have an
# x86_64 counterpart, otherwise the universal package would silently ship a
# single-architecture file.
while IFS= read -r -d '' source; do
  relative="${source#"$ARM64_DIR"/}"
  counterpart="$X64_DIR/$relative"
  destination="$OUT_DIR/$relative"
  mkdir -p "$(dirname "$destination")"

  if [[ -L "$source" ]]; then
    cp -a "$source" "$destination"
    copied=$((copied + 1))
    continue
  fi

  if [[ ! -f "$counterpart" ]]; then
    if is_arch_specific "$relative"; then
      cp -a "$source" "$destination"
      copied=$((copied + 1))
      arch_only+=("$relative (arm64)")
      continue
    fi
    echo "error: $relative exists in the arm64 package but not the x86_64 one" >&2
    exit 1
  fi

  if is_macho "$source" && is_macho "$counterpart"; then
    # Each package contributes its own architecture's slice, whatever it
    # happened to contain.
    extract_slice "$counterpart" x86_64 "$SLICE_DIR/x86_64"
    extract_slice "$source" arm64 "$SLICE_DIR/arm64"
    lipo -create "$SLICE_DIR/x86_64" "$SLICE_DIR/arm64" -output "$destination"
    # lipo does not carry the mode across.
    if [[ -x "$source" ]]; then
      chmod +x "$destination"
    fi
    merged=$((merged + 1))
  else
    cp -a "$source" "$destination"
    copied=$((copied + 1))
    if [[ "$relative" != "build-info.json" ]] && ! cmp -s "$source" "$counterpart"; then
      differing+=("$relative")
    fi
  fi
done < <(find "$ARM64_DIR" -mindepth 1 \( -type f -o -type l \) -print0)

# Files present only in the x86_64 tree would be dropped by the walk above.
while IFS= read -r -d '' source; do
  relative="${source#"$X64_DIR"/}"
  if [[ -e "$ARM64_DIR/$relative" ]]; then
    continue
  fi
  if is_arch_specific "$relative"; then
    mkdir -p "$(dirname "$OUT_DIR/$relative")"
    cp -a "$source" "$OUT_DIR/$relative"
    copied=$((copied + 1))
    arch_only+=("$relative (x86_64)")
    continue
  fi
  echo "error: $relative exists in the x86_64 package but not the arm64 one" >&2
  exit 1
done < <(find "$X64_DIR" -mindepth 1 \( -type f -o -type l \) -print0)

if [[ ${#arch_only[@]} -gt 0 ]]; then
  echo "per-architecture resources carried through from one package only:"
  printf '  %s\n' "${arch_only[@]}"
fi

if [[ ${#differing[@]} -gt 0 ]]; then
  echo "warning: architecture-independent files differ between the two packages:" >&2
  printf '  %s\n' "${differing[@]}" >&2
fi

# build-info.json was copied from the arm64 package and still claims that
# architecture. Correct it so the shipped metadata matches what is inside.
BUILD_INFO="$OUT_DIR/build-info.json"
/usr/bin/sed -i '' \
  -e "s|\"target\": \".*apple-darwin\"|\"target\": \"$UNIVERSAL_TARGET\"|" \
  -e "s|\"platform\": \"macos-[^\"]*\"|\"platform\": \"$UNIVERSAL_PLATFORM\"|" \
  "$BUILD_INFO"
if ! grep -q "\"target\": \"$UNIVERSAL_TARGET\"" "$BUILD_INFO" \
  || ! grep -q "\"platform\": \"$UNIVERSAL_PLATFORM\"" "$BUILD_INFO"; then
  echo "error: could not rewrite target/platform in $BUILD_INFO" >&2
  exit 1
fi

# Every Mach-O in the published tree must now carry both slices.
incomplete=()
while IFS= read -r -d '' file; do
  if ! is_macho "$file"; then
    continue
  fi
  archs="$(lipo -archs "$file")"
  if [[ "$archs" != *x86_64* || "$archs" != *arm64* ]]; then
    incomplete+=("${file#"$OUT_DIR"/} [$archs]")
  fi
done < <(find "$OUT_DIR" -type f -print0)

if [[ ${#incomplete[@]} -gt 0 ]]; then
  echo "error: universal package contains single-architecture binaries:" >&2
  printf '  %s\n' "${incomplete[@]}" >&2
  exit 1
fi

echo "Universal macOS package: $OUT_DIR"
echo "  $merged Mach-O files merged, $copied resources copied"
lipo -archs "$OUT_DIR/FutureboardNative"
lipo -archs "$OUT_DIR/Chromium Embedded Framework.framework/Chromium Embedded Framework"
