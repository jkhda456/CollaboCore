<p align="center">
  <img src="CollaboCore_Icon.png" width="200">

<br>

<sub>
    Meet our mascot: a cute little friend from one of my games.
    <br>
    (Human-made! / untouched ANIMAL BOX)
  </sub>
</p>

<h1 align="center">collaboCore</h1>

<p align="center"><b>English</b> | <a href="readme.ko.md">한국어</a></p>

Easy on the admin, easy on the agent — an all-in-one sandbox machine.

📦 **Download:** [CollaboIDE Releases](https://github.com/jkhda456/CollaboIDE/releases)


## ⚡ Easy to use

Just run the launcher, and you're done!

<p align="center">
  <img src="screenshot.png" width="600">

<br>

## 🌀 Comfort

 * A WASM build of the Linux kernel ([tombl/linux](https://github.com/tombl/linux), 7.1) runs as a native process.
 * Network, logging and sharing your files, all in one place.
 * Handing your agent a root shell is safe.


## 🚀 Just change launcher.conf

Write only what you want in `launcher.conf` in the runtime folder. Say you want to hand your project to
a Claude Code agent but decide where it may reach:

```ini
# My project as /work, reference data read-only
mount = ~/projects/my-app:/work
mount = ~/datasets:/data:ro

# Only these hosts are reachable (everything else is blocked)
allow = api.anthropic.com
allow = github.com
allow = pypi.org
allow = files.pythonhosted.org

# Log every request with its time
log-file = logs/network.log

# Claude Code in the guest; the key stays on the host and the guest never sees it
addon = claude-code
addon-config = claude-code:apiKey=${ANTHROPIC_API_KEY}
```

`./launcher --dry-run` shows the command line it would run (with the key hidden); every key is
listed by `./launcher --help` and in the comments of the `launcher.conf` that ships with it.



## 📐 Collabo IDE example

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
                                                     └ guest: busybox, CPython 3.13 + pip, curl, ssh, git, screen,
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
  that TLS with a per-session CA the guest trusts). The same overlay carries **GNU screen** 5.0
  (sessions, windows, detach and reattach), ported to a guest that has no `fork()`.
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
| `userspace/` | musl, compiler-rt, busybox, `/init` and the guest tools (`hfetch`, `hostcall`, `collabo-agentd`) → `initramfs.cpio`; `patches/` has our busybox patch (vi edits UTF-8 text, e.g. Korean) |
| `python/` | CPython 3.13 cross-built for the guest, its C libraries, Rust for the guest (pydantic-core, jiter), pip and the openai SDK → `python.cpio`; `lib/` has `collabo_core` and `mmap` |
| `nettools/` | curl, dropbear (ssh), git and GNU screen for the guest, with distro's patches (screen's is ours) → `tools.cpio` |
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

./build.sh                  # everything: ~25 min the first time (kernel and CPython dominate)
./build.sh --help           # the steps and commands below
```

Everything else — Node, CMake, Ninja, Binaryen, the Rust toolchain, the two upstream clones
(`kernel/linux`, `third_party/distro`), a Rust nightly and every source and wheel the guest Python
is built from (checksum-pinned in `python/sources.lock` and `python/packages.lock`) — is
fetched by the build itself, without root.

```sh
./build.sh engine web runtime         # only these steps
./build.sh test --all                 # unit, guest-boot, end-to-end, Dart and Flutter checks
./build.sh release                    # dist/release: archives, SHA256SUMS, VERSION
./build.sh clean [dist|build|all]     # remove build output (DRY=1 shows what would go)
./build.sh export DIR                 # copy just the source, ready to commit
```

`./build.sh` alone builds tools, kernel, userspace, python, engine, web and runtime. The guest's
network tools (curl, ssh, git, screen) and the add-ons are separate steps; run them after `python` and
before `engine`, or the runtime ships without them:

```sh
./nettools/build.sh                   # -> nettools/out/tools.cpio
./addons/build.sh                     # -> addons/*/out/*.cpio
./build.sh engine runtime             # pick them up (the bundled add-ons only: addons/README.md)
```

The scripts call each other directly, so they need their executable bits: a checkout that lost
them (a commit made on Windows, a copy through a shared folder) needs a `chmod +x` on the scripts
first. `.gitattributes` keeps scripts, patches and the guest's `/etc` in LF on every platform.

The kernel and the guest images are the same everywhere and are built once here. The engine is
native code, so **each platform builds its own** with `./build.sh runtime` (=
`scripts/package-runtime.sh`; on Windows run `scripts/package-runtime-windows.ps1`, which needs
no bash); the CI workflow does that for six platforms and runs the tests there.

Try it without an app:

```sh
cd dist/runtime/collabo-core-linux-x64        # the paths below are relative to it
bin/collabo-core-engine --kernel app/images/vmlinux.wasm \
  --initramfs app/images/initramfs.cpio --initramfs app/images/python.cpio \
  --mount "$OLDPWD:/work"                     # a root shell in this terminal; Ctrl-] then q leaves
```

Or, with no arguments at all, `./launcher` (`launcher.exe`) in the runtime folder: it starts the
engine with the manifest's images and the options in `launcher.conf` beside it — by default the
empty `work/` folder next to `bin/`, shared as `/work`. `./launcher --dry-run` shows the command line.
Add-ons are looked for in `app/images/addons` unless the manifest or `launcher.conf` names another
`addon-dir`, so `./launcher --addon NAME` works as it is.

`bin/collabo-core-engine --help` lists every option (`exec` for one command, the network policy,
add-ons, the terminal).

## More

[readme.detail.md](readme.detail.md) — what is implemented and verified, Flutter integration and
bundling, the full configuration, the guest's tools, the security model and the control protocol.
[docs/note.md](docs/note.md) — the running work log (Korean).
