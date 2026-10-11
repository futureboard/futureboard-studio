# `nix flake check`: smoke tests that run headless in the build sandbox.
{ pkgs, packages }:

let
  inherit (pkgs) runCommand;
in
{
  # The server must serve the real, embedded web UI — not the 503 "web UI is
  # not in this build" fallback — and every bundle the page loads must come
  # back byte-for-byte as Vite built it.
  livestage-server-webui =
    runCommand "livestage-server-webui-check"
      {
        nativeBuildInputs = [
          pkgs.curl
          pkgs.gnugrep
        ];
        webui = packages.livestage-server.webui;
      }
      ''
        export HOME=$TMPDIR
        ${packages.livestage-server}/bin/livestage-server \
          --daemon --no-autosave --http 127.0.0.1:18730 > server.log 2>&1 &
        server=$!
        trap 'kill $server 2>/dev/null || true' EXIT

        for _ in $(seq 1 100); do
          if curl -fsS http://127.0.0.1:18730/ -o index.html 2>/dev/null; then break; fi
          sleep 0.1
        done
        cat server.log
        test -s index.html || { echo "server did not answer"; exit 1; }
        cmp index.html $webui/index.html

        assets=$(grep -oE '/assets/[^"]+' index.html | sort -u)
        test -n "$assets"
        for asset in $assets; do
          curl -fsS "http://127.0.0.1:18730$asset" -o served
          cmp served "$webui$asset"
          echo "served $asset ($(stat -c %s served) bytes)"
        done
        touch $out
      '';

  # The CLI tools start and print their usage.
  apak-cli = runCommand "apak-cli-check" { } ''
    ${packages.apakinstaller}/bin/apak --help > apak.txt
    ${packages.apakinstaller}/bin/makeapak --help > makeapak.txt
    cat apak.txt makeapak.txt
    touch $out
  '';
}
