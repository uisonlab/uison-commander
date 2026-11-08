#!/bin/sh
# The macOS release build, signed (see tools/sign.sh: otherwise the Keychain asks for the password after
# each rebuild). Arguments go to cargo.
#
#   build_scripts/build.sh    → target/release/uison-commander
set -eu

cd "$(dirname "$0")/.."
cargo build --release -p commander "$@"
tools/sign.sh target/release/uison-commander
BIN=target/release/uison-commander
echo "$PWD/$BIN ($(du -h "$BIN" | cut -f1 | tr -d ' '), $(codesign -dvv "$BIN" 2>&1 | sed -n 's/^Authority=//p' | head -1))"
