# Every Futureboard native app as a Nix package (x86_64-linux).
#
# Each package mirrors how the existing Linux packaging builds and lays out
# that app (xtask `package`, packaging/linux, packaging/livestage); see
# nix/README.md for the package list and every build-time download decision.
{
  pkgs,
  self ? null,
  # The APAK trust anchor baked into apakinstaller/apak/makeapak. Release
  # builds refuse to build without one (apps/native/apakinstaller/build.rs).
  # The production key is a CI secret and `apak.public.key` is git-ignored, so
  # pass it with `.override { apakVerifyingKey = "..."; }`, or evaluate with
  # `--impure` and APAK_VERIFYING_KEY in the environment. Without either the
  # tools trust the public *development* key (see `developmentApakVerifyingKey`).
  apakVerifyingKey ? null,
  # The public account-service origin baked into SphereUIComponents. CI uses
  # this same default (FUTUREBOARD_AUTH_API_URL in .github/workflows).
  authApiUrl ? "https://futureboard.studio",
}:

let
  inherit (pkgs) lib stdenv;

  version = (lib.importJSON ../version.json).version;

  sources = pkgs.callPackage ./sources.nix { };
  src = pkgs.callPackage ./source.nix { inherit sources; };
  cef = pkgs.callPackage ./cef.nix { inherit sources; };
  prebuilt = pkgs.callPackage ./prebuilt.nix { inherit sources; };
  livestage-webui = pkgs.callPackage ./livestage-webui.nix {
    inherit version;
    # Refresh when bun.lock changes for the web UI (see nix/README.md).
    nodeModulesHash = "sha256-9yI89vp4lgRwkmk6Wodz89SjD6Ghm41nqlv3V4EWqvk=";
  };

  # Every crate in Cargo.lock (crates.io and the git dependencies), vendored
  # once and shared by all packages. This is nixpkgs' fetchCargoVendor (what
  # `cargoHash` uses) rather than `cargoLock.lockFile` + `outputHashes`:
  # importCargoLock vendors into a flat `<name>-<version>` directory and
  # cannot represent Cargo.lock's two `download-cef 2.3.2` packages (one from
  # crates.io, one from the cef-rs git repository); fetchCargoVendor keeps
  # each source in its own directory. Refresh `hash` whenever Cargo.lock
  # changes: set it to `lib.fakeHash`, build any package, copy the reported
  # hash.
  cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
    name = "futureboard-cargo-vendor";
    src = lib.fileset.toSource {
      root = ../.;
      fileset = lib.fileset.unions [
        ../Cargo.toml
        ../Cargo.lock
      ];
    };
    hash = "sha256-+ufOaDylHEDISvY6SqkEprbfiBoVX6x6GBVc6VVEruI=";
  };

  rustTarget = stdenv.hostPlatform.rust.cargoShortTarget;
  releaseDir = "target/${rustTarget}/release";

  # `TLWr9q…` is the Ed25519 public key of the development signing seed that
  # apps/native/apakinstaller/build.rs uses for non-release builds
  # (0x00…01). It only verifies packages signed with that public seed.
  developmentApakVerifyingKey = "TLWr9q15+/WrvMr8wmnYXNJlHtS4hbWGnyQa7fCluik=";
  impureApakKey = builtins.getEnv "APAK_VERIFYING_KEY";
  apakKey =
    if apakVerifyingKey != null then
      apakVerifyingKey
    else if impureApakKey != "" then
      impureApakKey
    else
      developmentApakVerifyingKey;
  apakTrustAnchor = if apakKey == developmentApakVerifyingKey then "development" else "configured";

  # Libraries GPUI/wgpu and the X11/Wayland stack open with dlopen at run time
  # (Vulkan loader, Wayland client, xkbcommon, Xlib extensions, GL/EGL). They
  # are prepended to the executables' RUNPATH instead of exported through
  # LD_LIBRARY_PATH, so they never leak into processes the apps launch
  # (browsers, third-party plug-in hosts' children, …).
  guiRuntimeLibs = with pkgs; [
    vulkan-loader
    wayland
    libxkbcommon
    libGL
    xorg.libX11
    xorg.libXcursor
    xorg.libXi
    xorg.libXrandr
    xorg.libxcb
    fontconfig
    freetype
    alsa-lib
  ];

  nativeBuildInputs = with pkgs; [
    pkg-config
    cmake
    perl
    rustPlatform.bindgenHook
  ];

  buildInputs = with pkgs; [
    alsa-lib
    dbus
    fontconfig
    freetype
    glib
    gtk4 # VST3/CLAP editor hosts (DirectAudioEngine, SpherePluginHost build.rs)
    libGL
    libxkbcommon
    openssl
    vulkan-loader
    wayland
    xorg.libX11
    xorg.libXcursor
    xorg.libXi
    xorg.libXrandr
    xorg.libxcb
    zlib
    zstd
  ];

  # Built-in plug-in cdylibs staged under Plugins/ (xtask `--plugin all`):
  # every workspace member under crates/BuiltinAudioPlugins/crates whose
  # library is a cdylib — the same discovery xtask does with cargo metadata.
  builtinPlugins =
    let
      dir = ../crates/BuiltinAudioPlugins/crates;
      members = (lib.importTOML ../Cargo.toml).workspace.members;
      entries = lib.attrNames (lib.filterAttrs (_: type: type == "directory") (builtins.readDir dir));
      manifestOf = name: dir + "/${name}/Cargo.toml";
      isPlugin =
        name:
        builtins.elem "crates/BuiltinAudioPlugins/crates/${name}" members
        && builtins.pathExists (manifestOf name)
        && builtins.elem "cdylib" ((lib.importTOML (manifestOf name)).lib.crate-type or [ ]);
      describe =
        name:
        let
          manifest = lib.importTOML (manifestOf name);
          package = manifest.package.name;
          libName = manifest.lib.name or (lib.replaceStrings [ "-" ] [ "_" ] package);
        in
        {
          inherit package;
          file = "lib${libName}.so";
        };
    in
    map describe (lib.filter isPlugin entries);

  # Prepend the GUI runtime libraries to every ELF executable in the given
  # directories (after the fixup phase has shrunk the RUNPATH).
  addGuiRunpath = dirs: ''
    for dir in ${lib.concatStringsSep " " dirs}; do
      for f in "$dir"/*; do
        if [ -f "$f" ] && [ ! -L "$f" ] && [ -x "$f" ] && isELF "$f"; then
          old=$(patchelf --print-rpath "$f")
          patchelf --set-rpath "${lib.makeLibraryPath guiRuntimeLibs}''${old:+:$old}" "$f"
        fi
      done
    done
  '';

  # Link a CEF runtime (libcef.so, paks, ICU data, V8 snapshots, locales/)
  # beside a CEF host executable, as xtask stages it flat beside the binary.
  linkCefRuntime = dest: ''
    for entry in ${cef}/*; do
      name=$(basename "$entry")
      case " ${lib.concatStringsSep " " cef.sdkOnlyEntries} " in
        *" $name "*) continue ;;
      esac
      ln -s "$entry" "${dest}/$name"
    done
  '';

  mkRustApp =
    {
      pname,
      cargoBuildFlags,
      buildFeatures ? [ ],
      buildNoDefaultFeatures ? false,
      needsCef ? false,
      needsCrashpad ? false,
      needsWebui ? false,
      needsApakKey ? false,
      postBuild ? "",
      installPhase,
      postFixup ? "",
      passthru ? { },
      meta,
    }:
    pkgs.rustPlatform.buildRustPackage {
      inherit
        pname
        version
        src
        cargoDeps
        cargoBuildFlags
        buildFeatures
        buildNoDefaultFeatures
        nativeBuildInputs
        buildInputs
        postBuild
        installPhase
        postFixup
        ;

      # `cmake` is only for crates whose build scripts drive CMake.
      dontUseCmakeConfigure = true;

      # The workspace test suites are run by CI with `cargo test`; they are
      # not repeated inside every package build.
      doCheck = false;

      env =
        {
          FUTUREBOARD_AUTH_API_URL = authApiUrl;
          # The Studio build script downloads ONNX Runtime unless told not to;
          # the Nix build links the prefetched copy beside the binary instead.
          FUTUREBOARD_ORT_SKIP_DOWNLOAD = "1";
        }
        // lib.optionalAttrs needsCef {
          # cef-dll-sys: an existing directory with a matching archive.json is
          # used as-is, never downloaded.
          CEF_PATH = "${cef}";
        }
        // lib.optionalAttrs needsCrashpad {
          # crashpad-rs-sys finds its `.crashpad-ok` marker here and links the
          # prebuilt libraries instead of downloading them; it then copies the
          # handler into the target directory, where the Studio build script's
          # crashpad-handler-bundler picks it up (as in CI).
          CRASHPAD_CACHE_DIR = "${prebuilt.crashpad}";
        }
        // lib.optionalAttrs needsApakKey {
          APAK_VERIFYING_KEY = apakKey;
        };

      preBuild =
        lib.optionalString needsWebui ''
          rm -rf apps/native/livestage/webui/dist
          cp -r ${livestage-webui} apps/native/livestage/webui/dist
          chmod -R u+w apps/native/livestage/webui/dist
        ''
        + lib.optionalString (needsApakKey && apakTrustAnchor == "development") ''
          echo "note: APAK tools trust the development key; see nix/README.md to bake the production key"
        '';

      passthru = passthru // {
        inherit apakTrustAnchor;
      };

      meta = {
        homepage = "https://github.com/arizkami/Futureboard";
        platforms = [ "x86_64-linux" ];
      }
      // meta;
    };

  # ── Plug-in host / scanner ───────────────────────────────────────────────
  plugin-host = mkRustApp {
    pname = "futureboard-plugin-host";
    cargoBuildFlags = [
      "-p"
      "sphere-plugin-host"
      "--bin"
      "FutureboardPluginHostX64"
      "--bin"
      "FutureboardPluginScanner"
    ];
    buildFeatures = [
      "plugin-host-bin"
      "plugin-scanner-bin"
    ];
    installPhase = ''
      runHook preInstall
      app=$out/libexec/futureboard-plugin-host
      mkdir -p $app $out/bin
      install -m755 ${releaseDir}/FutureboardPluginHostX64 ${releaseDir}/FutureboardPluginScanner $app/
      ln -s $app/FutureboardPluginHostX64 $out/bin/FutureboardPluginHostX64
      ln -s $app/FutureboardPluginScanner $out/bin/FutureboardPluginScanner
      runHook postInstall
    '';
    postFixup = addGuiRunpath [ "$out/libexec/futureboard-plugin-host" ];
    meta = {
      description = "Futureboard's out-of-process plug-in host and isolated plug-in scanner";
      mainProgram = "FutureboardPluginScanner";
    };
  };

  # ── Futureboard Studio ───────────────────────────────────────────────────
  # The app, its sidecars and the APAK tools are built in xtask's single
  # feature graph (cargo_build::build: the three packages selected together,
  # sidecar features merged in), then the built-in plug-in cdylibs
  # (plugins::build_plugins).
  #
  # Every binary is a fat-LTO, one-codegen-unit link, and linking
  # FutureboardNative and the apakinstaller GUI at the same time needs more
  # than 12 GiB. So the binaries are requested in two cargo runs over the
  # *same* package/feature selection: the first builds every shared artifact
  # and links the sidecars and APAK tools, the second only links the Studio
  # engine library (libFutureboardEngine_x64.so, which FutureboardNative loads
  # from its own directory) and the launcher. The result is identical to xtask's one invocation (CI
  # instead caps the whole build at CARGO_BUILD_JOBS=2).
  studioPackages = [
    "-p"
    "futureboard_native"
    "-p"
    "sphere-plugin-host"
    "-p"
    "apakinstaller"
  ];
  studioFeatures = [
    "sphere-plugin-host/plugin-host-bin"
    "sphere-plugin-host/plugin-scanner-bin"
  ];

  studio = mkRustApp {
    pname = "futureboard-studio";
    cargoBuildFlags = studioPackages ++ [
      "--bin"
      "FutureboardPluginHostX64"
      "--bin"
      "FutureboardPluginScanner"
      "--bin"
      "apakinstaller"
      "--bin"
      "apak"
      "--bin"
      "makeapak"
    ];
    buildFeatures = studioFeatures;
    needsCef = true;
    needsCrashpad = true;
    needsApakKey = true;

    postBuild = ''
      cargo build -j "$NIX_BUILD_CORES" --offline --profile release --target ${rustTarget} \
        ${lib.concatStringsSep " " studioPackages} \
        --features ${lib.concatStringsSep "," studioFeatures} \
        --lib --bin FutureboardNative

      cargo build -j "$NIX_BUILD_CORES" --offline --profile release --target ${rustTarget} \
        ${lib.concatMapStringsSep " " (plugin: "-p ${plugin.package}") builtinPlugins}
    '';

    installPhase = ''
      runHook preInstall

      app=$out/libexec/futureboard-studio
      mkdir -p $app/bin $app/Plugins $app/Resources $out/bin

      install -m755 \
        ${releaseDir}/FutureboardNative \
        ${releaseDir}/libFutureboardEngine_x64.so \
        ${releaseDir}/FutureboardPluginHostX64 \
        ${releaseDir}/FutureboardPluginScanner \
        $app/
      install -m755 ${releaseDir}/apakinstaller ${releaseDir}/apak ${releaseDir}/makeapak $app/bin/
      install -m755 ${prebuilt.crashpad}/bin/crashpad_handler $app/crashpad_handler
      ln -s ${prebuilt.onnxruntime}/lib/libonnxruntime.so $app/libonnxruntime.so
      ${linkCefRuntime "$app"}
      ${lib.concatMapStringsSep "\n" (
        plugin: "install -m755 ${releaseDir}/${plugin.file} $app/Plugins/"
      ) builtinPlugins}

      cat > $app/build-info.json <<EOF
      {
        "schemaVersion": 1,
        "application": "Futureboard Studio",
        "binary": "FutureboardNative",
        "sidecars": ["FutureboardPluginHostX64", "FutureboardPluginScanner"],
        "edition": "community",
        "profile": "release",
        "target": "${stdenv.hostPlatform.rust.rustcTarget}",
        "platform": "linux-x64",
        "version": "${version}",
        "gitCommit": ${if self != null && self ? rev then ''"${self.rev}"'' else "null"},
        "gitDirty": ${if self != null && self ? rev then "false" else "null"},
        "builtAtUtc": "${
          if self != null && self ? lastModifiedDate then self.lastModifiedDate else "19700101000000"
        }",
        "rustcVersion": "${pkgs.rustc.version}",
        "cargoVersion": "${pkgs.cargo.version}",
        "packager": "nix"
      }
      EOF

      ln -s $app/FutureboardNative $out/bin/futureboard-studio
      ln -s $app/FutureboardNative $out/bin/FutureboardNative

      # Desktop integration, as packaging/aur installs it.
      install -Dm644 ${../packaging/linux/futureboard-studio.desktop} \
        $out/share/applications/futureboard-studio.desktop
      substituteInPlace $out/share/applications/futureboard-studio.desktop \
        --replace-fail 'Exec=FutureboardNative %U' 'Exec=futureboard-studio %U'
      install -Dm644 ${../packaging/linux/futureboard-studio-mime.xml} \
        $out/share/mime/packages/futureboard-studio.xml
      install -Dm644 ${../packages/shared/app/icons/app.png} \
        $out/share/icons/hicolor/512x512/apps/futureboard-studio.png

      runHook postInstall
    '';

    postFixup = addGuiRunpath [
      "$out/libexec/futureboard-studio"
      "$out/libexec/futureboard-studio/bin"
    ];

    passthru = {
      inherit cef builtinPlugins;
      inherit (prebuilt) crashpad onnxruntime;
    };

    meta = {
      description = "Futureboard Studio, the native GPUI digital audio workstation";
      mainProgram = "futureboard-studio";
    };
  };

  # ── LiveStage ────────────────────────────────────────────────────────────
  # Default features: the GPUI desktop mixer (`LiveStage`) and the server
  # (`livestage-server`) with third-party plug-ins through the plug-in host,
  # both with the web UI embedded.
  livestage = mkRustApp {
    pname = "livestage";
    cargoBuildFlags = [
      "-p"
      "livestage"
      "--bin"
      "LiveStage"
      "--bin"
      "livestage-server"
    ];
    needsCef = true; # sphere_ui_components/builtin-plugin-editor links cef-rs
    needsWebui = true;
    installPhase = ''
      runHook preInstall
      app=$out/libexec/livestage
      mkdir -p $app $out/bin
      install -m755 ${releaseDir}/LiveStage ${releaseDir}/livestage-server $app/
      # External plug-ins run in Futureboard's plug-in host, which LiveStage
      # looks for beside its own executable.
      ln -s ${plugin-host}/libexec/futureboard-plugin-host/FutureboardPluginHostX64 $app/
      ln -s ${plugin-host}/libexec/futureboard-plugin-host/FutureboardPluginScanner $app/
      ln -s $app/LiveStage $out/bin/LiveStage
      ln -s $app/livestage-server $out/bin/livestage-server
      runHook postInstall
    '';
    postFixup = addGuiRunpath [ "$out/libexec/livestage" ];
    passthru = {
      webui = livestage-webui;
    };
    meta = {
      description = "LiveStage live mixer: GPUI desktop app and server with the embedded web UI";
      mainProgram = "LiveStage";
    };
  };

  # Headless server: `--no-default-features` (ALSA and the built-in effects
  # only), the configuration apps/native/livestage/Cargo.toml documents for a
  # Linux box with no desktop.
  livestage-server = mkRustApp {
    pname = "livestage-server";
    cargoBuildFlags = [
      "-p"
      "livestage"
      "--bin"
      "livestage-server"
    ];
    buildNoDefaultFeatures = true;
    needsWebui = true;
    installPhase = ''
      runHook preInstall
      install -Dm755 ${releaseDir}/livestage-server $out/bin/livestage-server
      runHook postInstall
    '';
    passthru = {
      webui = livestage-webui;
    };
    meta = {
      description = "LiveStage headless server (built-in effects, ALSA, embedded web UI)";
      mainProgram = "livestage-server";
    };
  };

  # The appliance's binaries, built exactly as packaging/livestage/make-image.sh
  # builds them: `--no-default-features --features appliance`.
  livestage-appliance = mkRustApp {
    pname = "livestage-appliance";
    cargoBuildFlags = [
      "-p"
      "livestage"
      "--bin"
      "livestage-server"
      "--bin"
      "livestage-setup"
      "--bin"
      "livestage-installer"
    ];
    buildNoDefaultFeatures = true;
    buildFeatures = [ "appliance" ];
    needsWebui = true;
    installPhase = ''
      runHook preInstall
      mkdir -p $out/bin
      install -m755 \
        ${releaseDir}/livestage-server \
        ${releaseDir}/livestage-setup \
        ${releaseDir}/livestage-installer \
        $out/bin/
      runHook postInstall
    '';
    meta = {
      description = "LiveStage appliance tools: server, first-boot console (livestage-setup) and disk installer";
      mainProgram = "livestage-setup";
    };
  };

  # ── Smaller apps ─────────────────────────────────────────────────────────
  jamsession = mkRustApp {
    pname = "futureboard-jam";
    cargoBuildFlags = [
      "-p"
      "jamsession"
      "--bin"
      "FutureboardJam"
    ];
    installPhase = ''
      runHook preInstall
      install -Dm755 ${releaseDir}/FutureboardJam $out/bin/FutureboardJam
      runHook postInstall
    '';
    postFixup = addGuiRunpath [ "$out/bin" ];
    meta = {
      description = "Futureboard Audio Jam standalone client";
      mainProgram = "FutureboardJam";
    };
  };

  singer = mkRustApp {
    pname = "futureboard-singer";
    cargoBuildFlags = [
      "-p"
      "singer"
      "--bin"
      "Singer"
    ];
    installPhase = ''
      runHook preInstall
      install -Dm755 ${releaseDir}/Singer $out/bin/Singer
      runHook postInstall
    '';
    postFixup = addGuiRunpath [ "$out/bin" ];
    meta = {
      description = "Singer, the native instrument playground for the Solfege engine";
      mainProgram = "Singer";
    };
  };

  apakinstaller = mkRustApp {
    pname = "apakinstaller";
    cargoBuildFlags = [
      "-p"
      "apakinstaller"
      "--bin"
      "apakinstaller"
      "--bin"
      "apak"
      "--bin"
      "makeapak"
    ];
    needsApakKey = true;
    installPhase = ''
      runHook preInstall
      mkdir -p $out/bin
      install -m755 ${releaseDir}/apakinstaller ${releaseDir}/apak ${releaseDir}/makeapak $out/bin/
      runHook postInstall
    '';
    postFixup = addGuiRunpath [ "$out/bin" ];
    meta = {
      description = "APAK package installer (GUI) and the apak / makeapak command-line tools";
      mainProgram = "apakinstaller";
    };
  };

  # The dedicated CEF subprocess executable. Futureboard's own packaging only
  # ships it on macOS (Linux/Windows Studio runs CEF helpers as itself); it is
  # built here so every workspace app has a package.
  cef-helper = mkRustApp {
    pname = "futureboard-cef-helper";
    cargoBuildFlags = [
      "-p"
      "futureboard_cef_helper"
      "--bin"
      "futureboard_cef_helper"
    ];
    needsCef = true;
    installPhase = ''
      runHook preInstall
      app=$out/libexec/futureboard-cef-helper
      mkdir -p $app $out/bin
      install -m755 ${releaseDir}/futureboard_cef_helper $app/
      ${linkCefRuntime "$app"}
      ln -s $app/futureboard_cef_helper $out/bin/futureboard_cef_helper
      runHook postInstall
    '';
    meta = {
      description = "Futureboard's dedicated CEF helper process";
      mainProgram = "futureboard_cef_helper";
    };
  };

  apps = {
    inherit
      studio
      livestage
      livestage-server
      livestage-appliance
      jamsession
      singer
      apakinstaller
      cef-helper
      plugin-host
      ;
  };
in
apps
// {
  default = studio;

  all = pkgs.linkFarm "futureboard-all-${version}" (
    lib.mapAttrsToList (name: path: { inherit name path; }) apps
  );

  # Building blocks, exposed so their hashes can be refreshed on their own
  # (`nix build .#livestage-webui`, `nix build .#cef`).
  inherit cef livestage-webui;
}
