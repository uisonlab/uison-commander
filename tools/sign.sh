#!/bin/sh
# Signs a commander binary with an Apple Development identity, so the Keychain knows it across rebuilds.
#
#   tools/sign.sh target/release/uison-commander
#
# Why not ad-hoc or self-signed: a Keychain item belongs to the partition of the code that made it —
# `teamid:<Team ID>` for code signed with a Team ID, `cdhash:<hash of this very file>` for anything else
# (self-signed included). Without a Team ID every rebuild is a stranger and the system asks for the login
# password again. Checked 08.11.2026: store, rebuild, re-sign, read — read silently only with a Team ID.
#
# The identity: UISON_SIGN_IDENTITY (a name or part of it), by default the personal Apple Development
# certificate. Without it the binary keeps its ad-hoc signature (a warning, not an error).
set -eu

BIN=${1:?usage: tools/sign.sh <binary>}
NAME=${UISON_SIGN_IDENTITY:-"Apple Development: codecity@icloud.com"}
ID=com.marketkernel.uison-commander

[ "$(uname)" = Darwin ] || exit 0
# By its SHA-1, of the valid ones only: an expired certificate of the same name is not ambiguous then.
SHA=$(security find-identity -v -p codesigning | grep -F "\"$NAME" | head -1 | awk '{print $2}')
if [ -z "$SHA" ]; then
    echo "warning: no valid code signing identity \"$NAME\": $BIN stays ad-hoc signed," \
        "the Keychain will ask for the password after each rebuild" >&2
    exit 0
fi
codesign --force --sign "$SHA" --identifier "$ID" --timestamp=none "$BIN" 2>&1 | grep -v 'replacing existing signature' >&2 || true
codesign --verify "$BIN"
