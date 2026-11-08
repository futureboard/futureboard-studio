# Security Policy

Futureboard Studio is a native digital audio workstation that loads
third-party plug-ins, opens files from other people, talks to a few online
services and, through LiveStage, can serve a control surface on the network.
This document says which versions get security fixes, how to report a
problem, and where the trust boundaries are.

## Supported versions

Futureboard Studio ships on a rolling, date-versioned beta schedule
(`YYYY.M.D-betaN…`, see `version.json`). Security fixes land on `main` and go
out in the next release; older releases are not patched.

| Build                                             | Security fixes |
| ------------------------------------------------- | -------------- |
| Latest release on GitHub Releases                 | Yes            |
| `nightly` release (rebuilt daily from `main`)     | Yes            |
| Any earlier release                               | No — update    |
| Third-party forks and modified builds             | No             |

**Editions.** This repository is the Community Edition (MIT). The Professional
Edition adds private sources (licensing, ASIO) on top of the same code base.
Reports about either edition go through the same channel below; the
Professional-only parts are handled privately.

## Reporting a vulnerability

**Please do not open a public issue, pull request or discussion for a
security problem.**

Report it privately through GitHub's private vulnerability reporting:

- <https://github.com/futureboard/futureboard-studio/security/advisories/new>
  (or the **Security → Report a vulnerability** button on the repository).

Please include:

- the affected version (Help → About, or `version.json`) and edition;
- the operating system and, for plug-in issues, the plug-in and its format;
- what an attacker has to control (a file the user opens, a plug-in, a
  machine on the same network, a website the user visits, …);
- steps or a minimal file/project that reproduces it, and what happens.

What to expect:

- **Acknowledgement within 3 business days.**
- An initial assessment and, where it is confirmed, a fix plan.
- **Coordinated disclosure within 90 days** of the report. We may ask to move
  the date when a fix needs more time, or publish sooner once a fix has shipped.
  We will credit you in the advisory unless you prefer otherwise.

Please give us a reasonable chance to fix the issue before disclosing it, do
not access or modify other people's data, and do not run tests against
Futureboard online services (accounts, licensing, Audio Jam) beyond what is
needed to show the issue.

## Security model and trust boundaries

### Plug-ins are trusted native code

VST3, VST2, CLAP, AU and ARA plug-ins are third-party native code. Loading
a plug-in means running its code **with your full user rights**. Futureboard
does not sandbox plug-ins and cannot protect you from a malicious one.
Install plug-ins only from vendors you trust.

- **Out-of-process hosting.** By default VST3, VST2, CLAP, AU and the
  built-in plug-ins run in a separate host process (`FutureboardPluginHostX64`),
  one per plug-in module. Plug-in scanning runs each bundle in its own
  short-lived scanner process with a timeout. This isolates **crashes and
  hangs**; it is not a privilege boundary, as the host runs as the same user
  with no restricted token or sandbox.
- **ARA runs in-process.** ARA 2 plug-ins (for example Melodyne) are loaded
  directly into the Studio engine process because ARA requires it. An ARA
  plug-in crash takes Studio down with it.
- **Legacy in-process mode.** Setting `FUTUREBOARD_PLUGIN_LEGACY_IN_PROCESS=1`
  (a troubleshooting switch) loads every plug-in inside Studio instead.
- The Studio ↔ host control channel is the child process's inherited
  stdin/stdout pipe, and audio crosses per-instance shared memory in the
  current logon session. Neither is exposed to the network.

### Files you open are untrusted input

Projects (`.fbproj`), presets (`.pst`), themes, soundfonts, audio, MIDI,
video and imported Cubase XML may come from other people. Parsers are
expected to reject malformed input without memory corruption. A crash or
refusal to open a hostile file is a bug we want to hear about, and anything
beyond that is a vulnerability.

A project refers to plug-ins and media **by path**. Treat a project from an
unknown source the way you would treat any other document that can reference
files on your machine and on the network.

Locale (`.ftl`) files are compiled into the application and are not loaded
from disk at runtime.

### LiveStage web console and OSC

LiveStage's web console (`livestage-server`, `apps/native/livestage`) is a
plain HTTP + WebSocket server:

- **Desktop / `livestage-server`:** listens on `127.0.0.1:8730` by default.
  `--http ADDR:PORT` changes that (for example `0.0.0.0:8730` for the LAN),
  and `--no-http` turns it off.
- **LiveStage appliance image:** the web console is **open to the network
  (`0.0.0.0:8730`) by default** and is advertised over mDNS.
- **Authentication:** without a `users.json` the console runs in *open mode*.
  When the server listens on a network address, a browser on another machine
  must first enter a **setup code** (rate-limited). The code is shown on the
  appliance's home screen, or printed by `livestage-server`. Clients on the
  same machine (loopback) need no code. Create an admin user straight away:
  once a user exists, clients sign in with a name and PIN (PBKDF2-hashed,
  rate-limited) and receive a session token.
- **Host names:** requests must name the server by IP address, `localhost`,
  the machine's own name or `NAME.local`, or a name added with `--web-host`.
  Other names are refused, which blocks DNS-rebinding pages. The WebSocket
  also rejects cross-origin pages, and responses forbid framing.
- **No TLS.** PINs, tokens and console traffic are visible to anyone who can
  observe the network.
- **OSC remote control** (UDP, port 8000) is off by default and **does not
  require a login**. When enabled it listens on loopback only. To use it from
  controllers on the network, set `osc.network` and list the controllers'
  addresses in `osc.allow` (`remote.json`, or an admin's `set_remote`); it
  then binds to the web console's address and drops packets from any other
  address. A source address is not a credential: on a network where UDP
  sources can be forged, anyone there can still drive it.
- **Connection slots:** a connection must send its request within 10 seconds,
  and a web-console connection that has not logged in (or entered the setup
  code) within 2 minutes is closed. When every slot is taken, the server
  closes a connection that never logged in to make room, so logged-in
  operators can always connect.

Treat a LiveStage console that is reachable from a network as having control of
the show: mixer, recording, plug-in loading from the scanned catalog, and on
the appliance its storage and network settings.

### Online services and downloads

Studio talks to: the Futureboard account service (sign-in through your
browser), the licensing service (Professional), the GitHub Releases API
(update checks), GitHub and Hugging Face (optional model downloads), the Audio
Jam service, and TONE3000. All of these use HTTPS/WSS with normal certificate
validation, except Audio Jam media, which is not encrypted end to end.

Updates are downloaded from GitHub Releases over HTTPS and checked against the
SHA-256 digest that GitHub publishes for the asset before they are run.

Local listeners: a loopback-only port used to hand `fbrd://` links to an
already-running Studio, and a one-shot loopback callback during browser
sign-in.

### Licensing

Professional licenses are Ed25519-signed tokens bound to a machine and
verified against a public key built into the binary; any failure means "not
licensed". The signing keys never ship in a client build.

## Scope

**In scope**

- Code execution, memory corruption, or file read/write/deletion triggered
  by opening a project, preset, theme, package, soundfont, or media file.
- Paths in opened files that make Studio contact network hosts or read files
  the user did not choose.
- Weaknesses in the Studio ↔ plug-in-host IPC that let another process, or a
  plug-in, corrupt Studio's memory or act on a different plug-in instance.
- Unauthenticated or cross-site control of LiveStage: authentication bypass,
  CSRF, DNS rebinding, injection, or reaching more than a signed-in user's role
  allows.
- DLL search-order or binary planting that runs code from a location an
  ordinary user would not consider trusted.
- Flaws in license-token or package-signature verification, update integrity,
  or the sign-in flow (token leakage, session fixation).
- Secrets or signing material committed to this repository, and weaknesses in
  the build and release workflows.

**Out of scope**

- What a plug-in can do once it is loaded: it already has your user rights.
  Plug-in crashes that the out-of-process host contains as designed are also
  out of scope.
- Attacks that need administrator rights, or control of your user account,
  environment variables or install directory.
- Bypassing Professional licensing by patching the binary or tampering with
  a machine you control.
- LiveStage left in open mode on a network after the setup code has been
  shared, or used without user accounts on an untrusted network. Report it if
  the documented protections (setup code, user accounts, Host and Origin
  checks) can be bypassed.
- Denial of service that needs an already-authenticated LiveStage user, or a
  local resource-exhaustion crash with no further impact. Please still file
  these as ordinary bugs.
- Vulnerabilities in third-party plug-ins, audio drivers, or the operating
  system. Report those to their vendors.
- Third-party forks and modified distributions.

## Hardening notes for users

- **Install from the official source.** Download Futureboard Studio from the
  GitHub Releases page of `futureboard/futureboard-studio` (or the
  Professional download from your Futureboard account) and keep it updated.
- **Be selective with plug-ins.** Only install plug-ins from vendors you trust,
  and keep plug-in folders writable only by you or by administrators.
- **Open projects from people you trust.** If you receive a project from an
  unknown source, check its plug-ins and media before opening it. Consider
  blocking outbound SMB (TCP 445) at your firewall so network paths cannot
  reach the internet.
- **LiveStage:**
  - Keep the desktop server on its default `127.0.0.1` unless you need remote
    control.
  - Before exposing it (or on the appliance's first start), enter the setup
    code and create an admin user right away so it leaves open mode. Use
    PINs longer than four digits.
  - Run it on a dedicated, trusted show network. Never port-forward it to the
    internet; use a VPN for remote access.
  - Leave OSC off unless you need it, and only on an isolated network.
- **Leave troubleshooting switches off.** Do not set
  `FUTUREBOARD_PLUGIN_LEGACY_IN_PROCESS` or the plug-in view debug variables
  in everyday use; they reduce isolation or open a local debugging port.
- **Contributors:** never commit `.env`, signing keys, `issue.secret`,
  keystores or certificates. Release secrets belong in the CI secret store,
  and only public keys belong in client builds.
