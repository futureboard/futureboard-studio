# The Cargo workspace as the Rust builds see it: only the files Cargo and the
# build scripts read, plus the git submodules (pinned in ./sources.nix) put
# back where `.gitmodules` expects them.
#
# Restricting the file set keeps unrelated edits (docs, the web app, packaging
# for other platforms) from invalidating every Rust build.
{
  lib,
  runCommand,
  sources,
}:

let
  fs = lib.fileset;
  root = ../.;

  workspaceFiles = fs.difference (fs.unions [
    ../Cargo.toml
    ../Cargo.lock
    ../.cargo
    ../version.json
    ../apps/native
    ../crates
    ../xtask
    ../external/rustysynth
    ../packages/assets
    ../packages/keymaps
    ../packages/shared
  ]) (fs.unions (map fs.maybeMissing [
    # Never part of a git checkout, but present in a `path:` flake reference.
    ../apps/native/target
    ../apps/native/livestage/webui/node_modules
    ../apps/native/livestage/webui/dist
  ]));

  tree = fs.toSource {
    inherit root;
    fileset = workspaceFiles;
  };
in
runCommand "futureboard-workspace-src" { } ''
  cp -r ${tree} $out
  chmod -R u+w $out

  place() {
    rm -rf "$out/$2"
    mkdir -p "$(dirname "$out/$2")"
    cp -r "$1" "$out/$2"
    chmod -R u+w "$out/$2"
  }

  place ${sources.cpal} external/cpal
  place ${sources.vst3sdk} external/vst3sdk
  place ${sources.vst3_base} external/vst3sdk/base
  place ${sources.vst3_pluginterfaces} external/vst3sdk/pluginterfaces
  place ${sources.vst3_public_sdk} external/vst3sdk/public.sdk
  place ${sources.clap} external/clap
  place ${sources.clap-helpers} external/clap-helpers
  place ${sources.ara_sdk} external/ARA_SDK
  place ${sources.ara_api} external/ARA_SDK/ARA_API
  place ${sources.lucide} packages/shared/lucide
  place ${sources.tabler-icons} packages/shared/tabler-icons
''
