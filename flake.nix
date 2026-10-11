{
  description = "Futureboard native apps (Studio, LiveStage, Jam, Singer, APAK tools) built with Nix";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;

      # x86_64-linux only: CEF, the Crashpad prebuilt and the ONNX Runtime
      # release are pinned for linux-x64, and the web UI's node_modules hash is
      # per-architecture (see nix/README.md).
      systems = [ "x86_64-linux" ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      packagesFor = pkgs: import ./nix/packages.nix { inherit pkgs self; };
    in
    {
      packages = forAllSystems packagesFor;

      apps = forAllSystems (
        pkgs:
        let
          p = self.packages.${pkgs.stdenv.hostPlatform.system};
          app = drv: exe: description: {
            type = "app";
            program = "${drv}/bin/${exe}";
            meta = { inherit description; };
          };
        in
        rec {
          studio = app p.studio "futureboard-studio" "Futureboard Studio";
          default = studio;
          livestage = app p.livestage "LiveStage" "LiveStage desktop mixer";
          livestage-server = app p.livestage-server "livestage-server" "LiveStage headless server";
          livestage-setup = app p.livestage-appliance "livestage-setup" "LiveStage appliance console";
          livestage-installer = app p.livestage-appliance "livestage-installer" "LiveStage disk installer";
          jamsession = app p.jamsession "FutureboardJam" "Futureboard Audio Jam client";
          singer = app p.singer "Singer" "Singer instrument playground";
          apakinstaller = app p.apakinstaller "apakinstaller" "APAK package installer";
          apak = app p.apakinstaller "apak" "APAK command-line tool";
          makeapak = app p.apakinstaller "makeapak" "APAK package builder";
          plugin-scanner = app p.plugin-host "FutureboardPluginScanner" "Futureboard plug-in scanner";
        }
      );

      checks = forAllSystems (
        pkgs:
        import ./nix/checks.nix {
          inherit pkgs;
          packages = self.packages.${pkgs.stdenv.hostPlatform.system};
        }
      );

      devShells = forAllSystems (pkgs: {
        default = import ./nix/shell.nix {
          inherit pkgs;
          packages = self.packages.${pkgs.stdenv.hostPlatform.system};
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
