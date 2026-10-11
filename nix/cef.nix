# The pinned CEF minimal distribution, laid out the way cef-dll-sys expects a
# `CEF_PATH` (what `cargo run -p SphereWebView --example install_cef` stages
# into build/cef/150.0.11/cef_linux_x86_64): Release/ and Resources/ flattened
# into one directory, the SDK headers beside them, and an `archive.json`
# naming the archive. With CEF_PATH pointing here the cef-dll-sys build script
# never tries to download.
#
# The Chromium binaries are prebuilt for a generic glibc system, so they are
# patched to load their libraries from the store.
{
  lib,
  stdenv,
  autoPatchelfHook,
  sources,
  alsa-lib,
  at-spi2-atk,
  at-spi2-core,
  atk,
  cairo,
  cups,
  dbus,
  expat,
  glib,
  gtk3,
  libdrm,
  libgbm,
  libGL,
  libxkbcommon,
  nspr,
  nss,
  pango,
  systemd,
  vulkan-loader,
  xorg,
}:

stdenv.mkDerivation {
  pname = "futureboard-cef";
  version = "150.0.11";

  src = sources.cefArchive;

  nativeBuildInputs = [ autoPatchelfHook ];

  buildInputs = [
    alsa-lib
    at-spi2-atk
    at-spi2-core
    atk
    cairo
    cups
    dbus
    expat
    glib
    libdrm
    libgbm
    libxkbcommon
    nspr
    nss
    pango
    stdenv.cc.cc.lib
    xorg.libX11
    xorg.libXcomposite
    xorg.libXdamage
    xorg.libXext
    xorg.libXfixes
    xorg.libXrandr
    xorg.libxcb
    xorg.libxshmfence
  ];

  # Loaded with dlopen at run time rather than linked.
  runtimeDependencies = [
    (lib.getLib systemd) # libudev
    libGL
    vulkan-loader
    gtk3
  ];

  dontConfigure = true;
  dontBuild = true;
  # Release binaries are already stripped; stripping libcef.so again only
  # costs time.
  dontStrip = true;

  installPhase = ''
    runHook preInstall

    mkdir -p $out
    cp -r Release/. $out/
    cp -r Resources/. $out/
    cp -r include cmake libcef_dll CMakeLists.txt $out/
    if [ -f CREDITS.html ]; then cp CREDITS.html $out/; fi

    cat > $out/archive.json <<EOF
    {
      "type": "minimal",
      "name": "${sources.cefArchive.name}",
      "sha1": "$(sha1sum ${sources.cefArchive} | cut -d' ' -f1)"
    }
    EOF

    runHook postInstall
  '';

  passthru = {
    # Everything else in $out is the runtime a CEF host needs beside its
    # executable: CEF resolves resources and locales from the directory of
    # /proc/self/exe on Linux, exactly as xtask stages it ("flat with locales/").
    sdkOnlyEntries = [
      "include"
      "cmake"
      "libcef_dll"
      "CMakeLists.txt"
      "CREDITS.html"
      "archive.json"
    ];
  };

  meta = {
    description = "Chromium Embedded Framework 150.0.11 (prebuilt minimal distribution) for Futureboard";
    homepage = "https://cef-builds.spotifycdn.com/index.html";
    license = lib.licenses.bsd3;
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
    platforms = [ "x86_64-linux" ];
  };
}
