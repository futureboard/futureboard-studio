#!/usr/bin/env bash
# Shared submodule bootstrap for CI (no VST3 docs / full history).
set -euo pipefail

# Required for Community Edition builds / clippy / tests.
#
# `external/ARA_SDK` is here because ARA hosting is compiled on Windows and
# macOS: `SphereAraHost` pulls in `ara2-bridge-companion`, whose build script
# hard-fails without the SDK. It is a submodule *containing submodules* —
# ARA_API, ARA_Library and ARA_Examples — and the headers everything needs live
# in the nested ARA_API, so initializing only the outer one leaves an empty
# directory and the build still fails.
REQUIRED_SUBMODULES=(
  external/vst3sdk
  external/ARA_SDK
  external/clap
  external/clap-helpers
  packages/shared/tabler-icons
  packages/shared/lucide
)

# Optional: local path checkout when the workspace still patches cpal via
# `external/cpal`. Production CI also accepts the git patch in Cargo.toml
# (`git+https://github.com/futureboard/cpal`), so a missing clone must not fail
# the whole job when the patch does not need the path.
OPTIONAL_SUBMODULES=(
  external/cpal
)

echo "Syncing submodule URLs..."
git submodule sync -- "${REQUIRED_SUBMODULES[@]}" "${OPTIONAL_SUBMODULES[@]}" || true

echo "Initializing required submodules (shallow where safe)..."
git submodule update --init --force --depth=1 --checkout -- \
  external/clap \
  external/clap-helpers \
  packages/shared/tabler-icons \
  packages/shared/lucide

# VST3 SDK needs full checkout for nested SDK submodules.
git submodule update --init --force --checkout -- external/vst3sdk

git -C external/vst3sdk submodule update --init --force --checkout -- \
  base cmake pluginterfaces public.sdk tutorials vstgui4

# ARA SDK, then its nested ARA_API only. That one holds every header the build
# consumes (`ARAInterface.h` for the companion shim, `ARAVST3.h` for the VST3
# bridge and scanner); ARA_Library and ARA_Examples are ~10 MB of code nothing
# here compiles, so they stay uninitialized.
git submodule update --init --force --depth=1 --checkout -- external/ARA_SDK
git -C external/ARA_SDK submodule update --init --force --depth=1 --checkout -- ARA_API

# Fail loudly here rather than 20 minutes later inside a build script.
if [[ ! -f external/ARA_SDK/ARA_API/ARAInterface.h ]]; then
  echo "error: external/ARA_SDK/ARA_API is empty after init — the nested ARA_API submodule did not check out." >&2
  exit 1
fi

for path in "${OPTIONAL_SUBMODULES[@]}"; do
  if git submodule update --init --force --depth=1 --checkout -- "$path"; then
    echo "Optional submodule ready: $path"
  else
    echo "Optional submodule skipped (non-fatal): $path"
  fi
done

echo "Submodules ready."
