# Prebuilt native pieces that the Studio build scripts would otherwise fetch at
# build time. Each is a fixed-output download (./sources.nix) unpacked and
# patched for the store, then handed to the build through the environment
# variable its build script already honours.
{
  lib,
  stdenv,
  autoPatchelfHook,
  sources,
  zlib,
}:

{
  # crashpad-rs-sys (`prebuilt` feature) looks for
  #   $CRASHPAD_CACHE_DIR/prebuilt/<version>/<target>/.crashpad-ok
  # and only downloads when that marker is missing. Point CRASHPAD_CACHE_DIR
  # at $out and it links the static libraries from here. The handler
  # executable is also used directly (CRASHPAD_HANDLER) and shipped beside the
  # app, as xtask does.
  crashpad = stdenv.mkDerivation {
    pname = "crashpad-rs-prebuilt";
    version = "0.2.7";
    src = sources.crashpadArchive;
    sourceRoot = ".";

    nativeBuildInputs = [ autoPatchelfHook ];
    buildInputs = [
      stdenv.cc.cc.lib
      zlib
    ];

    dontConfigure = true;
    dontBuild = true;

    installPhase = ''
      runHook preInstall
      dir=$out/prebuilt/0.2.7/x86_64-unknown-linux-gnu
      mkdir -p $dir $out/bin
      cp bindings.rs *.a $dir/
      install -m755 crashpad_handler $out/bin/crashpad_handler
      ln -s $out/bin/crashpad_handler $dir/crashpad_handler
      touch $dir/.crashpad-ok
      runHook postInstall
    '';

    # Keep the static archives intact for the Rust link.
    dontStrip = true;

    meta = {
      description = "Prebuilt Crashpad client libraries and handler used by crashpad-rs v0.2.7";
      homepage = "https://github.com/bahamoth/crashpad-rs";
      license = lib.licenses.asl20;
      sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
      platforms = [ "x86_64-linux" ];
    };
  };

  # The ONNX Runtime the Stem Extractor loads at run time (`ort` load-dynamic).
  # The Studio build script would download this same release next to the
  # binary; the Nix build sets FUTUREBOARD_ORT_SKIP_DOWNLOAD=1 and links it in
  # beside the binary instead.
  onnxruntime = stdenv.mkDerivation {
    pname = "onnxruntime-bin";
    version = "1.23.2";
    src = sources.onnxruntimeArchive;

    nativeBuildInputs = [ autoPatchelfHook ];
    buildInputs = [ stdenv.cc.cc.lib ];

    dontConfigure = true;
    dontBuild = true;

    installPhase = ''
      runHook preInstall
      mkdir -p $out/lib
      cp -P lib/libonnxruntime.so* lib/libonnxruntime_providers_shared.so $out/lib/
      cp -r include $out/
      runHook postInstall
    '';

    meta = {
      description = "ONNX Runtime 1.23.2 (Microsoft's prebuilt CPU release)";
      homepage = "https://onnxruntime.ai";
      license = lib.licenses.mit;
      sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
      platforms = [ "x86_64-linux" ];
    };
  };
}
