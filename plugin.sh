#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-study.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-study" ;;
    *) rel="x86_64-apple-darwin/gray-study" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-study" ;;
    *) rel="x86_64-unknown-linux-musl/gray-study" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-study.exe" "$here/target/release/gray-study" "$here/target/debug/gray-study.exe" "$here/target/debug/gray-study"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-study binary not found in $here" >&2
exit 1
