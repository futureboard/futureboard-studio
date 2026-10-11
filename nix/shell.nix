# `nix develop`: the Rust toolchain, bun/node for the web UIs, and every
# native library and prefetched SDK the build scripts need, wired through the
# same environment variables the Nix packages use.
{ pkgs, packages }:

let
  inherit (packages.studio) cef crashpad onnxruntime;

  runtimeLibs = with pkgs; [
    vulkan-loader
    wayland
    libxkbcommon
    libGL
    xorg.libX11
    xorg.libXcursor
    xorg.libXi
    xorg.libXrandr
    xorg.libxcb
  ];
in
pkgs.mkShell {
  inputsFrom = [ packages.studio ];

  packages = with pkgs; [
    cargo
    rustc
    clippy
    rustfmt
    rust-analyzer
    bun
    nodejs
  ];

  # The packages build with these; a shell build should too.
  CEF_PATH = "${cef}";
  CRASHPAD_CACHE_DIR = "${crashpad}";
  FUTUREBOARD_ORT_SKIP_DOWNLOAD = "1";
  ORT_DYLIB_PATH = "${onnxruntime}/lib/libonnxruntime.so";
  FUTUREBOARD_AUTH_API_URL = "https://futureboard.studio";

  # `cargo run` binaries are not patched like the packaged ones, so give them
  # the dlopen'd GPU/windowing libraries here (dev shell only).
  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;

  shellHook = ''
    echo "Futureboard dev shell: rustc $(rustc --version | cut -d' ' -f2), bun $(bun --version)"
    echo "CEF_PATH=$CEF_PATH"
  '';
}
