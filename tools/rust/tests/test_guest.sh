#!/usr/bin/env bash
# The tools of tools.cpio written in Rust (and openssl), run in the guest: xz, zstd and 7z
# round trips, jq, and git-lfs (clean/smudge filters, a push to a file:// remote and a clone
# from it, ls-files, fsck, prune). Needs a built runtime (dist/runtime).
#
#   tools/rust/tests/test_guest.sh [RUNTIME_DIR]   TOOLS_IMAGE=...  another tools.cpio
#   NET=1 tools/rust/tests/test_guest.sh           also download LFS objects over HTTPS
#                                                  (huggingface.co, a public test repository)
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
RT="${1:-$(ls -d "$ROOT"/dist/runtime/collabo-core-*/ | head -1)}"
RT="${RT%/}"
IMG="${TOOLS_IMAGE:-$RT/app/images/tools.cpio}"
fails=0

guest() {  # guest SECONDS 'script' -> its output (after the prompt)
  local t="$1" out
  out="$(timeout "$t" "$RT/bin/collabo-core-engine" exec --kernel "$RT/app/images/vmlinux.wasm" \
    --initramfs "$RT/app/images/initramfs.cpio" --initramfs "$IMG" ${NETFLAG:---no-network} -- sh -c "$2" 2>&1)" || true
  sed -n '/^root@collabo:~# /,$p' <<<"$out" | sed '1s/^root@collabo:~# //' | tr -d '\r'
}

check() {  # check NAME OUTPUT PATTERN
  if grep -qE "$3" <<<"$2"; then
    echo "ok   $1"
  else
    echo "FAIL $1"; sed 's/^/     /' <<<"$2" | tail -15; fails=$((fails + 1))
  fi
}

out="$(guest 300 '
cd /tmp && head -c 300000 /dev/urandom > r && cat /etc/services /etc/services > t
for c in xz zstd; do $c -k -c r > r.$c && $c -d -c r.$c | cmp - r && echo "$c-ok"; done
xz -9e -c t | xzcat | cmp - t && echo xz9-ok
7z a -bd -mx=5 a.7z r t >/dev/null && mkdir x && cd x && 7z x -bd ../a.7z >/dev/null && cmp r ../r && cmp t ../t && echo 7z-ok')"
check "xz round trip" "$out" "^xz-ok"
check "zstd round trip" "$out" "^zstd-ok"
check "xz -9e text" "$out" "^xz9-ok"
check "7z archive and extract" "$out" "^7z-ok"

out="$(guest 120 '
echo "{\"a\":[1,2,{\"b\":\"x\"}],\"s\":\"한글\"}" | jq -c "[.a[0]+.a[1], .a[2].b, (.s|length), (.s|test(\"글$\"))]"
printf "%s\n" "[.[] | . * 2]" "[1,2]" "[2,4]" "" "def f: reduce .[] as \$x (0; . + \$x); f" "[1,2,3]" "6" > /tmp/t.test
jq --run-tests /tmp/t.test | tail -1')"
check "jq filter" "$out" '^\[3,"x",2,true\]'
check "jq --run-tests" "$out" "2 of 2 tests passed"

out="$(guest 60 'printf abc | openssl dgst -sha256')"
check "openssl dgst" "$out" "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"

out="$(guest 600 '
set -e
git config --global user.email t@example.com && git config --global user.name t
git config --global init.defaultBranch main
git lfs install >/dev/null && echo installed
cd /tmp && git init -q --bare remote.git && git init -q r && cd r
git lfs track "*.bin" >/dev/null
for i in 1 2 3; do head -c $((i * 40000)) /dev/urandom > f$i.bin; done
echo plain > plain.txt
git add . && git commit -qm one
git show HEAD:f1.bin | head -1
git lfs ls-files
git remote add origin file:///tmp/remote.git
git push -q origin main 2>&1 | tail -1
cd /tmp && git clone -q file:///tmp/remote.git c && cd c
cmp f2.bin /tmp/r/f2.bin && echo clone-content-ok
git lfs fsck
echo changed > f1.bin && git commit -qam two && git lfs status --porcelain; git push -q origin main 2>&1 | tail -1
git lfs prune --dry-run 2>&1 | tail -1
git lfs env | grep -c "^Endpoint=file:///tmp/remote.git"')"
check "git lfs install" "$out" "^installed"
check "git-lfs clean filter (pointer in the commit)" "$out" "^version https://git-lfs.github.com/spec/v1"
check "git lfs ls-files" "$out" " \* f3.bin"
check "git lfs push (file:// remote)" "$out" "Uploading LFS objects: 100% \(3/3\)"
check "git clone with the smudge filter" "$out" "^clone-content-ok"
check "git lfs fsck" "$out" "^Git LFS fsck OK"
check "git lfs env" "$out" "^1$"

if [[ -n "${NET:-}" ]]; then
  out="$(NETFLAG=" " guest 900 '
git lfs install >/dev/null; cd /tmp
GIT_LFS_SKIP_SMUDGE=1 git clone -q https://huggingface.co/hf-internal-testing/tiny-random-bert hf && cd hf
git lfs pull -I "*.bin" 2>&1 | tail -1
sha256sum pytorch_model.bin')"
  check "git lfs pull over HTTPS" "$out" "^9922e8996d0c7e24c7f4e7a5d9c5b7303549f4ee94de0f1138b103014b51be13"
fi

if [[ $fails -gt 0 ]]; then
  echo "FAIL: $fails check(s) failed"; exit 1
fi
echo "rust tools in the guest: all checks passed"
