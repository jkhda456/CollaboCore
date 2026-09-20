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
                                                     └ guest: busybox, CPython 3.13, hfetch, hostcall
```

The same kernel and guest images also run in a browser (`dist/web`), where the workspace is a zip in
IndexedDB.

- **Sandbox** — the guest sees only the folders you mount and the hosts you allow.
- **Local disks** — mount any host folder read-write or read-only; export one as a zip.
- **Managed network** — one policy for both paths: an allow/deny list, the host's own localhost
  blocked by default, **API keys injected by the host** (the guest never sees them), and every
  attempt reported as an event.
- **Python** — CPython 3.13 with 522 stdlib modules, plus a `collabo_core` module for host calls.
- **Host access** — the app exposes named functions; running host programs is `deny` / `ask` /
  `allow`, and `ask` reaches the app for every request.
- **One file to ship** — a 34 MB runtime folder: the engine, the kernel and the guest images.
  No Node.js, no container, no VM.

## Components

| Path | What it is |
|---|---|
| `engine/` | the runtime that ships: Rust + wasmtime. Boots the kernel, serves virtio console/fs/net/vsock, the HTTP request API, host functions, and the stdio control protocol |
| `kernel/` | builds the WebAssembly kernel (the tree itself is cloned) |
| `userspace/` | musl, compiler-rt, busybox, `/init` and the guest tools (`hfetch`, `hostcall`, `collabo-agentd`) → `initramfs.cpio` |
| `python/` | CPython 3.13 cross-built for the guest → `python.cpio`, and the guest's `collabo_core` module |
| `dart/collabo_core/` | the Dart package an app uses; `SandboxTools` exposes the sandbox as LLM tools |
| `flutter/collabo_core_demo/` | a desktop demo app, including how to bundle the runtime on all three platforms |
| `web/`, `host/` | the browser build and the host modules it shares with it |
| `runtime/src/` | the earlier Node implementation, kept as the reference the engine was ported from (not shipped) |
| `tests/`, `docs/note.md` | the checks, and a running log of decisions and pitfalls |

## Build

Ubuntu x64. Install the system packages once, then build:

```sh
sudo apt-get install -y make flex bison bc pkg-config libncurses-dev device-tree-compiler \
     wabt clang-19 lld-19 llvm-19 rsync python3 openssl git curl

./build.sh                 # everything: ~25 min the first time (kernel and CPython dominate)
./build.sh --help          # the steps and commands below
```

Everything else — Node, CMake, Ninja, Binaryen, the Rust toolchain, and the two upstream clones
(`kernel/linux`, `third_party/distro`) — is fetched into `.tools/` by the build itself, without root.

```sh
./build.sh engine web runtime       # only these steps
./build.sh test --all               # unit, guest-boot, end-to-end, Dart and Flutter checks
./build.sh release                  # dist/release: archives, SHA256SUMS, VERSION
./build.sh clean [dist|build|all]   # remove build output (DRY=1 shows what would go)
./build.sh export DIR               # copy just the source, ready to commit
```

The kernel and the guest images are the same everywhere and are built once here. The engine is
native code, so **each platform builds its own** with `scripts/package-runtime.sh`; the CI workflow
does that for six platforms and runs the tests there.

Try it without an app:

```sh
cd dist/runtime/collabo-core-linux-x64        # the paths below are relative to it
bin/collabo-core-engine --kernel app/images/vmlinux.wasm \
  --initramfs app/images/initramfs.cpio --initramfs app/images/python.cpio \
  --mount "$OLDPWD:/work"                     # a root shell in this terminal; Ctrl-C ends it
```
