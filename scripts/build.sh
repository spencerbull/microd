#!/usr/bin/env bash
# Release build + stable code signing. Signing microd with the self-signed
# `microd-dev` identity keeps its macOS Input Monitoring approval valid
# across rebuilds (TCC trusts the identity, not the binary hash).
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release
codesign --force --sign microd-dev target/release/microd
echo "signed:"
codesign -dv target/release/microd 2>&1 | grep -E '^(Identifier|Authority|Signature)' || true

if [ -d tray ]; then
  (cd tray && swiftc -O -o micro-tray main.swift)
  echo "tray built"
fi
