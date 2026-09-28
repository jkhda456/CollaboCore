# collaboCore

A Linux machine for AI agents, compiled to WebAssembly and embedded in your app.

A WebAssembly build of the Linux kernel ([tombl/linux](https://github.com/tombl/linux), 7.1) runs
inside a single native process. Booting drops straight into a root shell — no login, because the
machine is the isolation. A Flutter desktop app (macOS, Windows, Linux) starts it through the Dart
package `collabo_core`, runs commands in it, shares local folders with it, and decides what it may
reach on the network and on the host.

```
 Flutter app ── package:collabo_core ──stdio JSON──▶ collabo-core engine (Rust + wasmtime, 11 MB)
                                                     │ WebAssembly Linux kernel (one thread per CPU)
                                                     │  ├ virtio-console : the root shell
                                                     │  ├ virtio-fs      : local folders, mounted
                                                     │  ├ virtio-net     : sockets, through a policy
                                                     │  └ virtio-vsock   : 1024 agent exec
                                                     │                     1080 HTTP request API
                                                     │                     1081 host-function proxy
                                                     │                     1082 host ssh-agent
                                                     └ guest: busybox, CPython 3.13 + pip, curl, ssh, git,
                                                              hfetch, hostcall
```

The same kernel and guest images also run in a browser (`dist/web`), where the workspace is a zip in
IndexedDB.

- **Sandbox** — the guest sees only the folders you mount and the hosts you allow.
- **Local disks** — mount any host folder read-write or read-only; export one as a zip.
- **Managed network** — one policy for both paths: an allow/deny list, the host's own localhost
  blocked by default, **API keys injected by the host** (the guest never sees them), and every
  attempt reported as an event.
- **Python** — CPython 3.13 with the standard library as python.org ships it (269 of its 290
  modules; the rest are Windows/macOS-only or need fork, ctypes or a display): ssl and hashlib on
  OpenSSL, sqlite3, bz2/lzma, readline (over BSD libedit)/curses, venv, and `mmap` as a copy-based module. Preinstalled:
  **pip**, and the **openai** SDK with pydantic v2 and jiter — their Rust extensions compiled for the guest and
  built into the interpreter.
  `collabo_core.openai_client()` sends the SDK's requests through the host, which adds the key.
- **Network tools** — curl, ssh (dropbear) and git in an optional overlay, plus busybox's wget, nc
  and telnet. They go through the same policy and events; with `network.ask` the app decides about
  hosts its lists do not name, `sshAgent` lends the host's ssh-agent (private keys stay outside), and
  `secrets` also reach curl's, git's and Python's own HTTPS to those hosts (the engine terminates
  that TLS with a per-session CA the guest trusts).
- **Host access** — the app exposes named functions; running host programs is `deny` / `ask` /
  `allow`, and `ask` reaches the app for every request.
- **One folder to ship** — a 94 MB runtime folder: the engine (11 MB), the kernel and the guest
  images (Python with pip and its packages is 62 MB of it, the network tools 14 MB).
  No Node.js, no container, no VM.

## Components

| Path | What it is |
|---|---|
| `engine/` | the runtime that ships: Rust + wasmtime. Boots the kernel, serves virtio console/fs/net/vsock, the HTTP request API, host functions, and the stdio control protocol |
| `kernel/` | builds the WebAssembly kernel (the tree itself is cloned) |
| `userspace/` | musl, compiler-rt, busybox, `/init` and the guest tools (`hfetch`, `hostcall`, `collabo-agentd`) → `initramfs.cpio` |
| `python/` | CPython 3.13 cross-built for the guest, its C libraries, Rust for the guest (pydantic-core, jiter), pip and the openai SDK → `python.cpio`; `lib/` has `collabo_core` and `mmap` |
| `nettools/` | curl, dropbear (ssh) and git for the guest, with distro's patches → `tools.cpio` |
| `dart/collabo_core/` | the Dart package an app uses; `SandboxTools` exposes the sandbox as LLM tools |
| `flutter/collabo_core_demo/` | a desktop demo app, including how to bundle the runtime on all three platforms |
| `addons/` | optional overlays the app can boot with (`addons/README.md`); `claude-code`: Claude Code for the guest, with Anthropic or OpenAI-compatible (local) models |
| `web/`, `host/` | the browser build and the host modules it shares with it |
| `runtime/src/` | the earlier Node implementation, kept as the reference the engine was ported from (not shipped) |
| `tests/`, `docs/note.md` | the checks, and a running log of decisions and pitfalls |

## Build

Ubuntu x64. Install the system packages once, then build:

```sh
sudo apt-get install -y make flex bison bc pkg-config libncurses-dev device-tree-compiler \
     wabt clang-19 lld-19 llvm-19 rsync python3 openssl git curl xz-utils patch

python3 build.py deps       # what this machine is still missing
python3 build.py            # everything: ~25 min the first time (kernel and CPython dominate)
python3 build.py --help     # the steps and commands below
```

Everything else — Node, CMake, Ninja, Binaryen, the Rust toolchain, the two upstream clones
(`kernel/linux`, `third_party/distro`), a Rust nightly and every source and wheel the guest Python
is built from (checksum-pinned in `python/sources.lock` and `python/packages.lock`) — is
fetched by the build itself, without root.

```sh
python3 build.py engine web runtime       # only these steps
python3 build.py status                   # what each step has produced
python3 build.py test --all               # unit, guest-boot, end-to-end, Dart and Flutter checks
python3 build.py release                  # dist/release: archives, SHA256SUMS, VERSION
python3 build.py clean [dist|build|all]   # remove build output (DRY=1 shows what would go)
python3 build.py export DIR               # copy just the source, ready to commit
```

`build.py` runs every script through `bash`, so a checkout that lost its executable bits (a commit
made on Windows, a copy through a shared folder) builds anyway, and it puts the bits back on start.
`python3 build.py perms --git` records them in the git index for the next commit. `.gitattributes`
keeps scripts, patches and the guest's `/etc` in LF on every platform.

The kernel and the guest images are the same everywhere and are built once here. The engine is
native code, so **each platform builds its own** with `python3 build.py runtime` (or
`bash scripts/package-runtime.sh`; on Windows it runs `scripts/package-runtime-windows.ps1`,
which needs no bash); the CI workflow does that for six platforms and runs the tests
there.

Try it without an app:

```sh
cd dist/runtime/collabo-core-linux-x64        # the paths below are relative to it
bin/collabo-core-engine --kernel app/images/vmlinux.wasm \
  --initramfs app/images/initramfs.cpio --initramfs app/images/python.cpio \
  --mount "$OLDPWD:/work"                     # a root shell in this terminal; Ctrl-] then q leaves
```

## More

[readme.detail.md](readme.detail.md) — what is implemented and verified, Flutter integration and
bundling, the full configuration, the guest's tools, the security model and the control protocol.
[docs/note.md](docs/note.md) — the running work log (Korean).
