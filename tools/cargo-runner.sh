#!/bin/sh
# Cargo's runner (.cargo/config.toml): `cargo run` signs the commander binary before starting it (see
# tools/sign.sh); tests and other binaries start as they are.
set -eu

case "$(basename "$1")" in
uison-commander) "$(dirname "$0")/sign.sh" "$1" ;;
esac
exec "$@"
