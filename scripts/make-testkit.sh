#!/usr/bin/env bash
# A kit to verify the runtime on another machine (macOS, Windows, ARM Linux): the packaged
# runtime(s) + the portable end-to-end test + launch scripts.
#
# The runtime itself needs nothing installed. The *test* is a Node script (node:test), so the
# target needs Node 20+ on its PATH to run the kit; the sandbox shell script does not.
#
#   PLATFORMS="win-x64 darwin-arm64" scripts/make-testkit.sh [OUT_DIR]   (default dist/testkit)
#
# On the target:  Windows  run-tests-windows.cmd       macOS/Linux  sh run-tests.sh
# Both write test-result.txt next to themselves.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$ROOT/dist/testkit}"
PLATFORMS="${PLATFORMS:-win-x64 win-arm64 darwin-arm64 darwin-x64 linux-x64 linux-arm64}"

rm -rf "$OUT"; mkdir -p "$OUT/runtime" "$OUT/tests/fixtures" "$OUT/host"
for p in $PLATFORMS; do
  [[ -d "$ROOT/dist/runtime/collabo-core-$p" ]] || { echo "missing dist/runtime/collabo-core-$p (scripts/package-runtime.sh)" >&2; exit 1; }
  cp -r "$ROOT/dist/runtime/collabo-core-$p" "$OUT/runtime/"
done
cp "$ROOT/tests/runtime.e2e.mjs" "$OUT/tests/"
cp "$ROOT"/tests/fixtures/* "$OUT/tests/fixtures/"
cp "$ROOT/host/zip.js" "$OUT/host/"

cat > "$OUT/run-tests-windows.cmd" <<'EOF'
@echo off
rem collaboCore runtime test on Windows. Uses the bundled node.exe; nothing to install.
cd /d "%~dp0"
set COLLABO_CORE_RUNTIME=%~dp0runtime
set RT=runtime\collabo-core-win-x64
if /i "%PROCESSOR_ARCHITECTURE%"=="ARM64" set RT=runtime\collabo-core-win-arm64
echo Testing %RT% ... (about a minute)
where node >nul 2>nul || (echo This kit needs Node 20+ on PATH to run the test ^(nodejs.org^). & pause & exit /b 2)
node --test --test-concurrency=1 --test-timeout=240000 --test-reporter=spec tests\runtime.e2e.mjs > test-result.txt 2>&1
set RC=%ERRORLEVEL%
type test-result.txt
echo.
if %RC%==0 (echo ALL TESTS PASSED) else (echo SOME TESTS FAILED - please send test-result.txt)
pause
EOF
cat > "$OUT/shell-windows.cmd" <<'EOF'
@echo off
rem The sandbox's root shell in this console, with this folder mounted at /work. Ctrl-] quits.
cd /d "%~dp0"
set RT=runtime\collabo-core-win-x64
if /i "%PROCESSOR_ARCHITECTURE%"=="ARM64" set RT=runtime\collabo-core-win-arm64
"%RT%\bin\collabo-core-engine.exe" --kernel "%RT%\app\images\vmlinux.wasm" --initramfs "%RT%\app\images\initramfs.cpio" --initramfs "%RT%\app\images\python.cpio" --mount "%~dp0.:/work"
EOF
cat > "$OUT/run-tests.sh" <<'EOF'
#!/bin/sh
# collaboCore runtime test on macOS or Linux. The runtime is self-contained; the test script
# needs Node 20+ on PATH.
cd "$(dirname "$0")"
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) p=darwin-arm64 ;; Darwin-x86_64) p=darwin-x64 ;;
  Linux-x86_64) p=linux-x64 ;; Linux-aarch64) p=linux-arm64 ;;
  *) echo "no runtime for $(uname -s) $(uname -m)"; exit 1 ;;
esac
command -v node >/dev/null || { echo "this kit needs Node 20+ on PATH to run the test (nodejs.org)"; exit 2; }
# macOS marks downloaded files as quarantined; the engine would be blocked.
[ "$(uname -s)" = Darwin ] && xattr -dr com.apple.quarantine runtime 2>/dev/null
chmod +x "runtime/collabo-core-$p/bin/collabo-core-engine"
echo "Testing collabo-core-$p ... (about a minute)"
COLLABO_CORE_RUNTIME="$PWD/runtime" node --test --test-concurrency=1 --test-timeout=240000 --test-reporter=spec tests/runtime.e2e.mjs 2>&1 | tee test-result.txt
status=$?
[ $status = 0 ] && echo "ALL TESTS PASSED" || echo "SOME TESTS FAILED - please send test-result.txt"
exit $status
EOF
cat > "$OUT/README.txt" <<'EOF'
collaboCore runtime test kit
============================
Checks that the sandbox runtime works on this computer: boots the WebAssembly Linux machine, runs
commands, mounts a local folder, network policy and API-key injection (against a local HTTPS server),
host functions and host-program permission, zip export, shutdown.

Windows:      double-click run-tests-windows.cmd    (shell-windows.cmd opens the sandbox shell)
macOS/Linux:  sh run-tests.sh

Needs Node 20+ on PATH for the test script only (nodejs.org); the sandbox itself is
self-contained (bin/collabo-core-engine).

Result: test-result.txt. The test uses localhost only, except one DNS lookup the sandbox is
refused (no traffic leaves the machine). A firewall prompt may appear; "allow" is not
required for the tests.
EOF
# cmd.exe wants CRLF line endings.
for f in "$OUT"/*.cmd "$OUT/README.txt"; do sed -i 's/\r*$/\r/' "$f"; done
chmod +x "$OUT/run-tests.sh"
echo "== testkit: $OUT ($(du -sh "$OUT" | cut -f1)) for: $PLATFORMS"
