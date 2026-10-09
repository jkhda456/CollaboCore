//! git-lfs, Git Large File Storage, written again in Rust for the guest. It follows git-lfs
//! 3.8.0 (its commands, options, messages, configuration and protocols); HTTP goes through
//! libcurl. Its own integration tests (t/*.sh) are the reference.

mod attrs;
mod cli;
mod commands;
mod config;
mod endpoint;
mod errors;
mod fs;
mod gitcmd;
mod gitfilter;
mod gitobj;
mod gitscanner;
mod gourl;
mod creds;
mod curl;
mod http;
mod lfsapi;
mod ssh;
mod filter;
mod lfs;
mod locking;
mod mancontent;
mod pktline;
mod pointer;
mod subprocess;
mod tasklog;
mod tools;
mod tq;
mod wildmatch;
#[macro_use]
mod trace;

pub const VERSION: &str = "3.8.0";

/// The guest has no fork() (wasm call stacks cannot be copied), and its musl does not declare
/// it; std's Command only falls back to it when posix_spawn cannot do the job, which the
/// commands here never ask for (no pre_exec, uid/gid or process group changes). The symbol
/// is here so that std links; it fails as the kernel would.
#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn fork() -> libc::pid_t {
    unsafe { *libc::__errno_location() = libc::ENOSYS };
    -1
}

fn main() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let code = commands::run_main();
    commands::cleanup();
    use std::io::Write;
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
