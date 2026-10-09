# tools/rust — the guest's tools written again in Rust

One cargo workspace, built for the guest by `build.sh` (the `rust` step of `tools/build.sh`) into
`tools.cpio`:

| crate | binary | what |
|---|---|---|
| `archive` | `collabo-archive` | xz, zstd and 7z (and unxz, xzcat, lzma, unlzma, lzcat, unzstd, zstdcat, zstdmt, 7za, 7zr), picked by the name it runs as |
| `jq` | `jq` | the JSON processor, following jq's development tree (1.8) |
| `git-lfs` | `git-lfs` | Git LFS, following git-lfs 3.8.0: every command, its configuration, messages and protocols |

The codecs are pure Rust crates. git-lfs links libcurl (with OpenSSL and zlib) for HTTP: on the
guest the static libraries of `tools/build/curl-root` and `python/deps`, which `build.sh` hands to
`git-lfs/build.rs` as `COLLABO_CURL_LIBDIRS`; on the host the system's `libcurl.so.4`.

    tools/rust/build.sh            for the guest (needs userspace/sysroot, the guest's Rust toolchain,
                                   and tools/build.sh's curl step)
    HOST=1 tools/rust/build.sh     for this machine, into tools/build/rust/host/release
    TEST=1 tools/rust/build.sh     unit tests first

Fixes to crates.io crates are patches in `patches/<crate>-<version>-<what>.patch`, applied by
`patch-crates.sh` to a copy (the build works on `tools/build/rust/src`).

## Tests

- `tests/test_guest.sh [RUNTIME]` — all of them in the guest (`tests/run.sh --all` runs it);
  `NET=1` adds an HTTPS download of LFS objects.
- `tests/difftest.py {xz,zstd,7z} OUR_DIR REF_DIR` — output compared with the original tools.
- jq: `jq --run-tests` with jq's own `tests/*.test`; `tests/jq-difftest.py` and `tests/jq-fuzz.py`
  compare with a jq built from its sources.
- git-lfs: git-lfs's own integration tests (`t/t-*.sh` of the git-lfs 3.8.0 sources) run against
  this binary: build git-lfs's Go helpers (`make` in `t/`: lfstest-gitserver, lfs-ssh-echo, the
  credential helpers...) and put this `git-lfs` first on the PATH; all 825 tests of the 104 files
  pass.

## Notes for the guest

- There is no `fork()`: std's `Command` only uses `posix_spawn` here, and `git-lfs/src/main.rs`
  defines a `fork` that fails with ENOSYS so that std links.
- The engine's native wasm stack is shared with the kernel; a guest program recursing too deep
  kills the whole VM, so jq caps its evaluation depth on wasm (`JQ_MAX_EVAL_DEPTH`).
- The guest's ILP32 `timespec` has a private padding field: build it with `zeroed()` and fields.
