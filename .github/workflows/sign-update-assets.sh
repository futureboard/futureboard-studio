#!/usr/bin/env bash
# Signs every installer in a directory for the in-app updater (#268, #339).
#
# Usage: sign-update-assets.sh <asset-dir>
#
# For each installer `<asset>` (.exe, .dmg, .AppImage) this writes
# `<asset>.sig`: a raw 64-byte Ed25519 signature over
#
#   "futureboard-studio-update-v1\n" || <asset name> || "\n" || SHA-256(<asset>)
#
# with the digest as 32 raw bytes. That is exactly the message
# `apps/native/studio/src/updater.rs::signed_message` verifies; change both
# together. The asset name is signed, so the file must be uploaded under the
# name it has here.
#
# Environment (repository secrets):
#   FUTUREBOARD_UPDATE_SIGNING_KEY  the Ed25519 private key, PEM (PKCS#8), as
#                                   written by `openssl genpkey -algorithm ed25519`
#   FUTUREBOARD_UPDATE_PUBLIC_KEY   its public half as 64 hex characters, the
#                                   same value the build bakes into Studio
#
# Fails closed: a missing or malformed key, a key pair that does not match, an
# empty directory or a signature that does not verify all stop the release.
set -euo pipefail

dir="${1:?usage: sign-update-assets.sh <asset-dir>}"

if [[ -z "${FUTUREBOARD_UPDATE_SIGNING_KEY:-}" ]]; then
  echo "::error::secret FUTUREBOARD_UPDATE_SIGNING_KEY is not set; installers cannot be signed and no build could install this release as an update." >&2
  exit 1
fi
public_hex="$(printf '%s' "${FUTUREBOARD_UPDATE_PUBLIC_KEY:-}" | tr -d '[:space:]' | tr 'A-F' 'a-f')"
if [[ ! "$public_hex" =~ ^[0-9a-f]{64}$ ]]; then
  echo "::error::secret FUTUREBOARD_UPDATE_PUBLIC_KEY must be the Ed25519 public key as 64 hex characters." >&2
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
umask 077

# Bytes from a hex string, written to stdout (bash printf emits NUL bytes).
hex_to_bin() {
  printf '%b' "$(printf '%s' "$1" | sed 's/../\\x&/g')"
}

printf '%s\n' "$FUTUREBOARD_UPDATE_SIGNING_KEY" > "$work/signing.pem"
if ! openssl pkey -in "$work/signing.pem" -noout 2>/dev/null; then
  echo "::error::FUTUREBOARD_UPDATE_SIGNING_KEY is not a PEM private key." >&2
  exit 1
fi

# The last 32 bytes of an Ed25519 SubjectPublicKeyInfo are the raw key.
derived_hex="$(openssl pkey -in "$work/signing.pem" -pubout -outform DER | tail -c 32 | od -An -v -tx1 | tr -d ' \n')"
if [[ "$derived_hex" != "$public_hex" ]]; then
  echo "::error::FUTUREBOARD_UPDATE_SIGNING_KEY does not belong to FUTUREBOARD_UPDATE_PUBLIC_KEY; builds would reject every signature." >&2
  exit 1
fi

# Verify with the public key exactly as baked into the build, rebuilt from its
# hex (SubjectPublicKeyInfo prefix for Ed25519 + the 32 key bytes).
hex_to_bin "302a300506032b6570032100${public_hex}" > "$work/public.der"
openssl pkey -pubin -inform DER -in "$work/public.der" -out "$work/public.pem"

shopt -s nullglob nocaseglob
assets=("$dir"/*.exe "$dir"/*.dmg "$dir"/*.appimage)
shopt -u nocaseglob
if [[ ${#assets[@]} -eq 0 ]]; then
  echo "::error::no installers (.exe, .dmg, .AppImage) in $dir to sign." >&2
  exit 1
fi

for asset in "${assets[@]}"; do
  name="$(basename "$asset")"
  # GitHub rewrites some characters in uploaded asset names; the signature
  # binds the name, so refuse anything that might not survive the upload.
  if [[ ! "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
    echo "::error::asset name '$name' may be renamed on upload, which would invalidate its signature." >&2
    exit 1
  fi
  {
    printf 'futureboard-studio-update-v1\n%s\n' "$name"
    openssl dgst -sha256 -binary "$asset"
  } > "$work/message"
  openssl pkeyutl -sign -inkey "$work/signing.pem" -rawin -in "$work/message" -out "$asset.sig"
  size="$(wc -c < "$asset.sig" | tr -d ' ')"
  if [[ "$size" != "64" ]]; then
    echo "::error::signature for $name is $size bytes, expected 64." >&2
    exit 1
  fi
  if ! openssl pkeyutl -verify -pubin -inkey "$work/public.pem" -rawin \
      -in "$work/message" -sigfile "$asset.sig" >/dev/null; then
    echo "::error::signature for $name does not verify against FUTUREBOARD_UPDATE_PUBLIC_KEY." >&2
    exit 1
  fi
  echo "signed: $name -> $name.sig"
done
