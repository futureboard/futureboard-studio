# LiveStage's browser UI (apps/native/livestage/webui, React + Vite, part of
# the root bun workspace). livestage/build.rs embeds `webui/dist` into the
# server, so the Rust builds copy this derivation's output there first.
#
# The npm dependencies come from the root `bun.lock` through a fixed-output
# derivation (the only step with network access), installed for this one
# workspace with `--filter`. When bun.lock changes in a way that affects the
# web UI, refresh `nodeModulesHash`: set it to `lib.fakeHash`, build
# `.#livestage-webui`, and copy the hash Nix reports.
{
  lib,
  stdenv,
  stdenvNoCC,
  autoPatchelfHook,
  bun,
  nodejs,
  version,
  nodeModulesHash ? "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
}:

let
  fs = lib.fileset;
  root = ../.;

  # Bun checks every workspace manifest against bun.lock, so all of them must
  # be present even though only the LiveStage UI is installed.
  manifests = fs.unions [
    ../package.json
    ../bun.lock
    (fs.fileFilter (file: file.name == "package.json") ../apps)
    (fs.fileFilter (file: file.name == "package.json") ../packages)
    (fs.fileFilter (file: file.name == "package.json") ../crates)
    (fs.fileFilter (file: file.name == "package.json") ../extensions)
  ];

  manifestSrc = fs.toSource {
    inherit root;
    fileset = fs.difference manifests (
      fs.unions (
        map fs.maybeMissing [
          ../node_modules
          ../apps/native/livestage/webui/node_modules
        ]
      )
    );
  };

  webuiSrc = fs.toSource {
    inherit root;
    fileset = fs.unions [
      (fs.difference ../apps/native/livestage/webui (
        fs.unions (
          map fs.maybeMissing [
            ../apps/native/livestage/webui/node_modules
            ../apps/native/livestage/webui/dist
          ]
        )
      ))
      # futureboard-theme.ts reads Studio's colours from here.
      ../packages/shared/themes
    ];
  };

  nodeModules = stdenvNoCC.mkDerivation {
    pname = "livestage-webui-node-modules";
    inherit version;
    src = manifestSrc;

    nativeBuildInputs = [ bun ];

    dontConfigure = true;

    buildPhase = ''
      runHook preBuild
      export HOME=$TMPDIR
      export BUN_INSTALL_CACHE_DIR=$TMPDIR/bun-cache
      bun install \
        --frozen-lockfile \
        --ignore-scripts \
        --no-progress \
        --no-save \
        --linker=hoisted \
        --filter ./apps/native/livestage/webui
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      mkdir -p $out
      cp -R node_modules $out/node_modules
      if [ -d apps/native/livestage/webui/node_modules ]; then
        cp -R apps/native/livestage/webui/node_modules $out/webui_node_modules
      fi
      # Bun's own bookkeeping is not needed to run the tools and is not
      # guaranteed to be byte-for-byte stable.
      rm -rf $out/node_modules/.cache $out/node_modules/.bun-tag
      runHook postInstall
    '';

    # Nothing in a fixed-output path may refer to the store.
    dontFixup = true;

    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
    outputHash = nodeModulesHash;
  };
in
stdenv.mkDerivation {
  pname = "livestage-webui";
  inherit version;
  src = webuiSrc;
  sourceRoot = "source/apps/native/livestage/webui";
  # The hoisted node_modules goes into the (otherwise read-only) workspace root.
  postUnpack = "chmod u+w source";

  # The Vite/TypeScript toolchains ship native executables and Node addons
  # built for generic glibc systems; patch them so they run in the sandbox.
  nativeBuildInputs = [
    autoPatchelfHook
    bun
    nodejs
  ];
  buildInputs = [ stdenv.cc.cc.lib ];

  configurePhase = ''
    runHook preConfigure
    # Hoisted packages live in the workspace root's node_modules (four levels
    # up from the web UI), where Node's resolver finds them.
    cp -R ${nodeModules}/node_modules ../../../../node_modules
    chmod -R u+w ../../../../node_modules
    if [ -d ${nodeModules}/webui_node_modules ]; then
      cp -R ${nodeModules}/webui_node_modules node_modules
      chmod -R u+w node_modules
    fi
    patchShebangs ../../../../node_modules node_modules 2>/dev/null || true
    # bun.lock carries no libc constraint, so the musl builds of the native
    # bindings are installed next to the glibc ones; they are never loaded.
    find ../../../../node_modules -maxdepth 2 -type d -name '*-linux-x64-musl' -prune -exec rm -rf {} +
    autoPatchelf ../../../../node_modules
    runHook postConfigure
  '';

  buildPhase = ''
    runHook preBuild
    export HOME=$TMPDIR
    bun run build
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    test -f dist/index.html
    cp -R dist $out
    runHook postInstall
  '';

  dontFixup = true;

  passthru = { inherit nodeModules; };

  meta = {
    description = "LiveStage web UI (static assets embedded by livestage-server)";
    platforms = lib.platforms.linux;
  };
}
