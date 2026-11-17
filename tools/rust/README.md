# tools/rust — the guest's tools written again in Rust

One cargo workspace, built for the guest by `build.sh` (the `rust` step of `tools/build.sh`) into
`tools.cpio`:

| crate | binary | what |
|---|---|---|
| `archive` | `collabo-archive` | xz, zstd, lz4, 7z and zip (and unxz, xzcat, lzma, unlzma, lzcat, unzstd, zstdcat, zstdmt, unlz4, lz4cat, 7za, 7zr), picked by the name it runs as |
| `data` | `collabo-data` | the data format tools, as busybox: jq, plutil and yq, picked by the name it runs as (the image links `/usr/bin/jq`, `plutil`, `yq` to it) or by the first argument (`collabo-data plutil -p x.plist`); `collabo-data --list` |
| `jq` | (library) | the JSON processor, following jq's development tree (1.8) |
| `plutil` | (library) | Apple's plutil and CoreFoundation's property lists, following swift-corelibs-foundation 6.4.2 (below) |
| `yq` | (library) | YAML, JSON, XML, ... following mikefarah/yq 4.54.1 |
| `git-lfs` | `git-lfs` | Git LFS, following git-lfs 3.8.0: every command, its configuration, messages and protocols |

A tool of `collabo-data` is a library crate with `pub fn main(argv: Vec<String>) -> i32`
(argv[0] the name it runs as); a new one is a member of the workspace, a dependency of `data`, a
name in its `APPLETS`, and a link in `tools/build.sh` (and `APPLETS_DATA` in `build.sh`).

plutil follows Apple's own code, Apache-2.0 (`plutil/LICENSE-swift-corelibs-foundation`): the
Swift plutil (`Sources/plutil`: commands, options, messages, exit statuses, key paths, `-p`, the
Swift and Obj-C literals) and CoreFoundation's property lists (`CFPropertyList.c`: the XML parser
with its error messages and line numbers, and the writer; `CFBinaryPList.c`: the binary reader's
checks and the writer's object order, uniquing and integer sizes; `CFOldStylePList.c`: OpenStep and
`.strings`), with JSON as `JSONSerialization` reads and writes it. Input is told apart as CF does
(binary, XML, OpenStep, then JSON). On purpose it differs in: no "Running in Swift mode" line in
`-help`; `-p` shows data as NSData does (the Swift code drops leading zeros and skips bytes of long
data); keyed-archiver UIDs convert, as CF writes them (the Swift code refuses them); JSON of a single
value (`-extract key json`) is written as a fragment (Foundation stops the program); JSON numbers of
more than 17 digits are Doubles (Foundation's NSDecimalNumber); a dictionary keeps its file's order
(CF's is its hash table's), which shows in binary and compact JSON only, the other formats sort; an
XML plist's declared encoding is one of UTF-8/16/32, US-ASCII and ISO-8859-1. libplist (LGPL-2.1)
was a reference for the formats and is the second implementation `tests/plutil-difftest.py`
compares against.

The codecs are pure Rust crates; lz4's is our port of lz4 1.10.0's library (`archive/src/lz4/`:
the fast compressor, the mid, hash-chain and optimal compressors of levels 2..12, the frame
format), whose frames are the original's byte for byte at every level and option, and whose
decoder fails where the original's does; its command line is a port of lz4's (BSD-2-Clause and
GPL-2.0-or-later, `archive/LICENSE-lz4-lib`, `archive/COPYING-lz4-programs`). zip (`archive/src/zip/`)
is Info-ZIP's Zip 3.0 written again: its command line, modes, file selection, messages, exit codes
and the archive's structure (UT/ux extra fields, the text bit, data descriptors, Zip64, traditional
encryption) as a Unix zip 3.0 makes them, an existing archive's entries copied as they are. Its
deflate data is zlib's, not Info-ZIP's own deflate, so the compressed bytes and the ratio shown can
differ a little. Not there: splits (-s), fixing (-F/-FF), self-extractors (-A/-J), copy mode (-U),
difference archives (-DF), log files (-lf), the progress displays (-db/-dc/-dd/...); -ic does nothing,
as on a Unix zip 3.0. Its help and license texts are Info-ZIP's (`archive/LICENSE-infozip`), and it
says it is not an Info-ZIP release, as that license asks. git-lfs links libcurl (with OpenSSL and zlib) for HTTP: on the
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
- `tests/difftest.py {xz,zstd,lz4,7z} OUR_DIR REF_DIR` — output compared with the original tools
  (lz4's reference: 1.10.0's single-threaded build, `make -C programs lz4 HAVE_MULTITHREAD=0`).
- `tests/zip-difftest.py OUR_DIR REF_DIR` — zip against Info-ZIP's zip 3.0 built from zip30.tar.gz
  (`make -f unix/Makefile generic`): 88 cases of messages, exit codes and archives record by record
  (what depends on the deflate bytes masked), each archive also tested by Info-ZIP's unzip.
- jq: `jq --run-tests` with jq's own `tests/*.test`; `tests/jq-difftest.py` and `tests/jq-fuzz.py`
  compare with a jq built from its sources.
- plutil: `tests/plutil-difftest.py files OUR_PLUTIL PLISTUTIL DIR` — every file (libplist's
  `test/data`: XML, binary with 1–8 byte references and offsets, OpenStep, `.strings`, JSON, UIDs,
  signed and unsigned integers) through our `-convert xml1` and `binary1`, and libplist's
  `plistutil -s -f xml` of the original and of both of ours must be the same; two files are known
  differences, where CF and libplist disagree. `tests/plutil-difftest.py random OUR_PLUTIL N SEED`
  — random property lists written by Python's plistlib (binary and XML) through our xml1, binary1
  and json, read back by plistlib.
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
