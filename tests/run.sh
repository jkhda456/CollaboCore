#!/usr/bin/env bash
# All automated checks, no browser needed.
#   tests/run.sh           unit tests (host modules, web panels) + web import graph
#   tests/run.sh --boot    + guest boots: networking, workspace, Python (a few minutes)
#   tests/run.sh --all     + the desktop runtime (stdio protocol) and the Dart package
# Needs a built tree (./build.sh).
set -euo pipefail
T="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$T")"
export PATH="$ROOT/.tools/node/bin:$PATH"

echo "== import graph"; node "$T/check-release.mjs"

echo "== unit tests (bare imports resolved by the page's import map)"
if [[ ! -d "$T/node_modules/jsdom" || ! -d "$T/node_modules/fake-indexeddb" ]]; then
  (cd "$T" && echo '{"private":true,"type":"module"}' > package.json && npm install --silent jsdom fake-indexeddb)
fi
log="$(mktemp)"
if ! (cd "$ROOT" && node --import "$T/importmap-register.mjs" --test "$T"/*.test.mjs >"$log" 2>&1); then
  grep -E "^✖|Error|expected|actual" "$log" | head -30; echo "FAIL: unit tests"; exit 1
fi
grep -E "^ℹ (tests|pass|fail)" "$log"

echo "== engine unit tests (command line, network policy, request parsing)"
"$ROOT/engine/build.sh" --tests | grep -E "^test result:" || { echo "FAIL: engine unit tests"; exit 1; }

echo "== NodeFS with Windows' open-flag constants (emulated)"
(cd "$ROOT" && node --import "$T/win-fs-register.mjs" --test "$T/nodefs-windows.emul.mjs" 2>&1 | grep -E "^ℹ (pass|fail)")
(cd "$ROOT" && node --import "$T/win-fs-register.mjs" --test "$T/nodefs-windows.emul.mjs" >/dev/null 2>&1) || { echo "FAIL: NodeFS windows flags"; exit 1; }

MODE="${1:-}"
if [[ "$MODE" == "--boot" || "$MODE" == "--all" ]]; then
  echo "== guest boot: shell + HTTP through the host callback (mock)"
  out="$(NET=mock "$ROOT/userspace/test-boot.sh" "$ROOT/userspace/out/initramfs.cpio" \
    "wget -qO- --header 'x-test: 1' --post-data 'ping' http://api.example.test/v1/x")"
  echo "$out" | grep -q "^\[host-fetch\] POST https://api.example.test/v1/x" || { echo "$out" | tail -20; echo "FAIL: host did not see the request"; exit 1; }
  echo "$out" | grep -q "mock response for https://api.example.test/v1/x" || { echo "FAIL: guest did not get the response"; exit 1; }
  echo "guest -> host -> guest round trip ok (network device path)"

  echo "== guest boot: hfetch over vsock (request-level API, fake fetch)"
  out="$(API=mock "$ROOT/userspace/test-boot.sh" "$ROOT/userspace/out/initramfs.cpio" \
    "hfetch -d '{\"a\":1}' -H 'content-type: application/json' -H 'x-api-key: K' https://mock.test/v1/messages" \
    "hfetch -H 'cookie: x' https://mock.test/echo; echo rc=\$?" \
    "hfetch -f https://mock.test/status/404; echo rc=\$?" \
    "hfetch https://mock.test/big | wc -c")"
  echo "$out" | grep -q '"x-api-key":"K"' || { echo "$out" | tail -25; echo "FAIL: allowed headers/body did not reach fetch"; exit 1; }
  echo "$out" | grep -q 'header-not-allowed: request header "cookie" is not allowed' || { echo "FAIL: cookie was not refused"; exit 1; }
  echo "$out" | grep -q '^rc=2' || { echo "FAIL: refused request should exit 2"; exit 1; }
  echo "$out" | grep -q '^rc=22' || { echo "FAIL: -f on 404 should exit 22"; exit 1; }
  echo "$out" | grep -q '^204800' || { echo "FAIL: 200KB body not received intact"; exit 1; }
  echo "hfetch request-level API ok"

  echo "== guest boot: /work (virtiofs) exported as a zip, checked with Python's zipfile"
  zip="$(mktemp -d)/work.zip"
  WORK=1 WORK_OUT="$zip" "$ROOT/userspace/test-boot.sh" "$ROOT/userspace/out/initramfs.cpio" \
    "echo hello > a.txt && mkdir -p src/deep && echo 'int main(){}' > src/deep/main.c && chmod 755 src/deep/main.c" \
    "cp /bin/busybox bb; ln -s src/deep/main.c link; mv a.txt b.txt; echo again >> b.txt" \
    "dd if=/dev/zero of=zeros bs=1024 count=2048 2>/dev/null" >/dev/null
  python3 "$T/verify-work-zip.py" "$zip" "$ROOT/userspace/out/busybox/bin/busybox"

  echo "== guest boot: the workspace survives a reload (saved zip -> next boot)"
  state="$(mktemp -d)/state.zip"
  WORK=1 WORK_STATE="$state" WORK_OUT=/dev/null "$ROOT/userspace/test-boot.sh" "$ROOT/userspace/out/initramfs.cpio" \
    "mkdir -p proj && echo 'saved in visit one' > proj/note.txt" >/dev/null
  out="$(WORK=1 WORK_STATE="$state" WORK_OUT=/dev/null "$ROOT/userspace/test-boot.sh" "$ROOT/userspace/out/initramfs.cpio" "cat /work/proj/note.txt")"
  echo "$out" | tr -d '\r' | grep -q '^saved in visit one$' || { echo "$out" | tail -15; echo "FAIL: workspace was not restored"; exit 1; }
  echo "restored after reload ok"

  echo "== guest boot: a full workspace refuses writes with ENOSPC and stays within its limit"
  out="$(WORK=1 WORK_MAX_BYTES=1000000 WORK_OUT=/dev/null "$ROOT/userspace/test-boot.sh" "$ROOT/userspace/out/initramfs.cpio" \
    "dd if=/dev/zero of=big bs=1024 count=2048 2>&1 | head -1")"
  echo "$out" | grep -q "No space left on device" || { echo "$out" | tail -15; echo "FAIL: no ENOSPC"; exit 1; }
  echo "capacity limit ok"

  if [[ -f "$ROOT/python/out/python.cpio" && -f "$ROOT/python/out/test.cpio" ]]; then
    PYCPIO="$ROOT/python/out/python.cpio:$ROOT/python/out/test.cpio"
    echo "== guest boot: CPython 3.13 stdlib smoke test (files, zlib, subprocess, threads, sockets, asyncio, memory, /work)"
    out="$(WORK=1 WORK_OUT=/dev/null EXTRA_CPIO="$PYCPIO" TEST_TIMEOUT_MS=400000 "$ROOT/userspace/test-boot.sh" \
      "$ROOT/userspace/out/initramfs.cpio" "ifconfig lo 127.0.0.1 up; python3 /usr/share/tests/smoke.py; echo smoke-rc=\$?" | tr -d '\r')"
    echo "$out" | grep -E "^(FAIL|SMOKE:|  FAILED)"
    echo "$out" | grep -q "^smoke-rc=0" || { echo "$out" | tail -25; echo "FAIL: python smoke test"; exit 1; }

    echo "== guest boot: python 'collabo_core' client against the host request bridge (fake fetch)"
    out="$(API=mock EXTRA_CPIO="$PYCPIO" TEST_TIMEOUT_MS=300000 "$ROOT/userspace/test-boot.sh" \
      "$ROOT/userspace/out/initramfs.cpio" "python3 /usr/share/tests/hostapi.py; echo hostapi-rc=\$?" | tr -d '\r')"
    echo "$out" | grep -E "^(FAIL|HOSTAPI:)"
    echo "$out" | grep -q "^hostapi-rc=0" || { echo "$out" | tail -25; echo "FAIL: collabo_core tests"; exit 1; }
  else
    echo "(python not built: skipping the Python guest tests; run python/build.sh)"
  fi
fi
if [[ "$MODE" == "--all" ]]; then
  echo "== desktop runtime: stdio protocol end to end (dist/runtime)"
  log="$(mktemp)"
  if ! node --test --test-concurrency=1 --test-timeout=240000 "$T/runtime.e2e.mjs" >"$log" 2>&1; then
    grep -E "^✖|Error|expected|actual" "$log" | head -30; echo "FAIL: runtime tests"; exit 1
  fi
  grep -E "^ℹ (tests|pass|fail)" "$log"

  echo "== Dart package (dart/collabo_core) against the runtime"
  DART="$ROOT/.tools/dart-sdk/bin/dart"
  [[ -x "$DART" ]] || { echo "no Dart SDK in .tools/dart-sdk (see scripts/bootstrap-tools.sh DART=1)"; exit 1; }
  (cd "$ROOT/dart/collabo_core" && "$DART" pub get >/dev/null && "$DART" analyze >/dev/null \
    && COLLABO_CORE_RUNTIME="$ROOT/dist/runtime" "$DART" test --concurrency=1 --timeout=5m 2>&1 | tail -1)
  (cd "$ROOT/dart/collabo_core" && COLLABO_CORE_RUNTIME="$ROOT/dist/runtime" "$DART" test --concurrency=1 --timeout=5m >/dev/null 2>&1) \
    || { echo "FAIL: dart tests"; exit 1; }

  echo "== Flutter demo app (flutter/collabo_core_demo): analyze + a real sandbox inside the Flutter engine"
  FLUTTER="$ROOT/.tools/flutter/bin/flutter"
  if [[ -x "$FLUTTER" ]]; then
    (cd "$ROOT/flutter/collabo_core_demo" && "$FLUTTER" pub get >/dev/null && "$FLUTTER" analyze >/dev/null \
      && COLLABO_CORE_RUNTIME="$ROOT/dist/runtime" "$FLUTTER" test 2>&1 | tail -1) | grep -q "All tests passed" \
      || { echo "FAIL: flutter demo"; exit 1; }
    echo "flutter demo ok"
  else
    echo "(no Flutter SDK in .tools/flutter: skipped)"
  fi
fi
echo "all checks passed"
