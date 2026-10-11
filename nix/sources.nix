# Fixed-output sources the Rust build needs but `Cargo.lock` does not cover.
#
# Git submodules: a flake only sees the superproject's tracked files, so every
# submodule the Linux build compiles against is pinned here at the exact commit
# recorded in the superproject (`git submodule status`). When a submodule is
# bumped, bump its `rev` here and refresh `hash` (set it to `lib.fakeHash`,
# build, copy the hash Nix reports).
#
# Prebuilt binaries: build scripts that would download at build time are fed
# these instead (see `nix/README.md` for each decision).
{
  lib,
  fetchFromGitHub,
  fetchurl,
}:

{
  # ── Submodules ───────────────────────────────────────────────────────────
  # `[patch.crates-io] cpal = { path = "external/cpal" }` (Linux ALSA fixes).
  cpal = fetchFromGitHub {
    owner = "futureboard";
    repo = "cpal";
    rev = "96c45f3c34c1d36f7835ac359878dd8a4da5a249";
    hash = "sha256-A7vNNfinwG1riGj+3MwRRAG+wJltRMIHNdelx7xETj0=";
  };

  # VST3 SDK superproject plus the three nested submodules the C++ bridges
  # compile (`base`, `pluginterfaces`, `public.sdk`). CI checks out the same
  # set; `vstgui4`, `doc` and `tutorials` are not compiled by anything here.
  vst3sdk = fetchFromGitHub {
    owner = "steinbergmedia";
    repo = "vst3sdk";
    rev = "58f8da7936800732561402d7936584ca4505de07";
    hash = "sha256-+6LBRu0BlriyxNCLlamtsJboPiyMABP5UZmB2Ctwnao=";
  };
  vst3_base = fetchFromGitHub {
    owner = "steinbergmedia";
    repo = "vst3_base";
    rev = "3d2e82f8e6bff59c1d8b7a27491a29c2286b5206";
    hash = "sha256-Pc1+03byLJ/ritBJ1VWtLoIbV8oPNDhEizDFYrXzUAY=";
  };
  vst3_pluginterfaces = fetchFromGitHub {
    owner = "steinbergmedia";
    repo = "vst3_pluginterfaces";
    rev = "31d6eeba6daaa3e2a8bfbe3e7a90ca0b7fbfbc1c";
    hash = "sha256-ZizVpDYQzGANtm7/OyFMxGyHXD5MTV58Iu2BcNlLCq0=";
  };
  vst3_public_sdk = fetchFromGitHub {
    owner = "steinbergmedia";
    repo = "vst3_public_sdk";
    rev = "a3911a4615dabbfdfd9d181ee26b05c70c289a95";
    hash = "sha256-NOaBH3Ej50M2hYwxbok1WrmrVdr0tnttDBuKfglTz6k=";
  };

  clap = fetchFromGitHub {
    owner = "free-audio";
    repo = "clap";
    rev = "ca1b43ee2b4f381e94e4170e622d60b92108b128";
    hash = "sha256-slvq7p15xCa7l2tvEaGPzDL8w6/8EI0DySC4Zp+c7tQ=";
  };
  clap-helpers = fetchFromGitHub {
    owner = "free-audio";
    repo = "clap-helpers";
    rev = "a61bcdf0ecc2c8db1e80bfe8bf9cb7e8d9fd2bbc";
    hash = "sha256-bqZxhi5zeaxcz3pzzyH/Jt4Tw8ae8kcNXSS0wbfaI80=";
  };

  # ARA hosting is Windows/macOS only, but the Linux VST3 bridge and scanner
  # take `kARAMainFactoryClass` from ARA_API when it is present (CI checks it
  # out), so it is included for parity. Only the nested ARA_API is compiled.
  ara_sdk = fetchFromGitHub {
    owner = "Celemony";
    repo = "ARA_SDK";
    rev = "1ca0b0aa65a73e932daa4a9402387a8afa5afd17";
    hash = "sha256-aW5lh9fvqP6LFNkDPNonROkXy3Y/yqmUjCt9/+tmwE0=";
  };
  ara_api = fetchFromGitHub {
    owner = "Celemony";
    repo = "ARA_API";
    rev = "b0905ce4428825909cf1477b4784a6ef7acc5e0d";
    hash = "sha256-ucfvzN8jUZye+O0mieC8a7EYV1u2dTGn2LveKT4zgUc=";
  };

  # SVG icons embedded with `include_str!` by SphereUIComponents. Only the
  # `icons/` trees are used, so fetch just those.
  lucide = fetchFromGitHub {
    owner = "lucide-icons";
    repo = "lucide";
    rev = "5b40f2c5a76a27eeb81c8f1b1c311121dee45495";
    sparseCheckout = [ "icons" ];
    hash = "sha256-3gXQahCrSuxYTmO1Kt9JWk0SWIjMprMdxrpHZTb1eyQ=";
  };
  tabler-icons = fetchFromGitHub {
    owner = "tabler";
    repo = "tabler-icons";
    rev = "6d128ed935d4546607b1e4d5d08c8b27bdbe7758";
    sparseCheckout = [ "icons" ];
    hash = "sha256-DrkaWDBqJ9ghuvqB2xD8q+hm3j2qRkqX89sFIMaGsb0=";
  };

  # ── Prebuilt binaries otherwise downloaded by build scripts ─────────────
  # CEF 150.0.11 (pinned by crates/SphereWebView/src/lib.rs and
  # .github/workflows/set-cef-path.sh). The minimal distribution is what
  # `install_cef` stages for CI (`archive.json` type "minimal").
  cefArchive = fetchurl {
    name = "cef_binary_150.0.11+gb887805+chromium-150.0.7871.115_linux64_minimal.tar.bz2";
    url = "https://cef-builds.spotifycdn.com/cef_binary_150.0.11%2Bgb887805%2Bchromium-150.0.7871.115_linux64_minimal.tar.bz2";
    hash = "sha256-qDDHnJ39TVTw67m7txiTUz7KYO0XcFS1nKyo2oDsQWI=";
  };

  # crashpad-rs-sys `prebuilt` strategy (what apps/native/studio selects for
  # x86_64 Linux) downloads this archive into its cache directory.
  crashpadArchive = fetchurl {
    url = "https://github.com/bahamoth/crashpad-rs/releases/download/v0.2.7/crashpad-0.2.7-x86_64-unknown-linux-gnu.tar.gz";
    hash = "sha256-F9ce58BmKnwQU0fh9PAfExoUuCMZJKgg2wMnoH6WcqY=";
  };

  # apps/native/studio/build.rs `ORT_DEFAULT_VERSION` (ort 2.0.0-rc.11 needs
  # an ONNX Runtime 1.23.x library).
  onnxruntimeArchive = fetchurl {
    url = "https://github.com/microsoft/onnxruntime/releases/download/v1.23.2/onnxruntime-linux-x64-1.23.2.tgz";
    hash = "sha256-H6TcrvIvb31c2BsowoAEFDUMEBFvX91GohYAglUcX5s=";
  };
}
