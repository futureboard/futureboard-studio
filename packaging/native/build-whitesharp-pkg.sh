#!/usr/bin/env bash
# The whitesharp macOS installer: packaging/native/build-plugin-pkg.sh whitesharp, with the same options.
exec "$(dirname "$0")/build-plugin-pkg.sh" whitesharp "$@"
