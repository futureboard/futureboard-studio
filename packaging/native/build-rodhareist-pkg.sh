#!/usr/bin/env bash
# The rodhareist macOS installer: packaging/native/build-plugin-pkg.sh rodhareist, with the same options.
exec "$(dirname "$0")/build-plugin-pkg.sh" rodhareist "$@"
