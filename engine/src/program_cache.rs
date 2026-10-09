//! Compiled guest programs on disk.
//!
//! Compiling a large guest program takes the host tens of seconds and gigabytes of memory (the wpe
//! add-on's browser is 170 MB of WebAssembly: about 20 s and 2.5 GB), and every new machine did it
//! again. A program of at least [`LARGE`] bytes is compiled once and kept, serialized, in the cache
//! directory, named by the SHA-256 of its bytes; later machines map that file instead, which takes
//! milliseconds, and its code is file-backed: shared by the machines that run it, and pages the
//! kernel can drop and read again. wasmtime checks that a file was made by the same wasmtime with
//! the same settings; one that was not is compiled again and replaced.
//!
//! The directory: `$COLLABO_PROGRAM_CACHE`, else `$XDG_CACHE_HOME/collabo-core/programs`, else
//! `~/.cache/collabo-core/programs` (`%LOCALAPPDATA%\collabo-core\programs` on Windows).
//! `COLLABO_PROGRAM_CACHE=off` turns it off. Files are only ever written by the engine; the oldest
//! unused ones go when the directory grows past [`BUDGET`].

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use wasmtime::{Engine, Module};

/// Smaller programs compile in well under a second; they stay in memory only.
pub const LARGE: usize = 32 << 20;
/// What the directory may hold before the least recently used files are deleted.
const BUDGET: u64 = 2 << 30;

/// Compiles `bytes`, or maps what an earlier machine compiled from the same bytes.
pub fn load_or_compile(engine: &Engine, bytes: &[u8]) -> Result<Module> {
    if bytes.len() < LARGE {
        return Module::new(engine, bytes);
    }
    let Some(dir) = cache_dir() else {
        return compile_large(engine, bytes);
    };
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let name: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let path = dir.join(format!("{name}.cwasm"));
    if path.is_file() {
        // SAFETY: the file is the engine's own serialization (written below, renamed into place
        // whole), in the user's own cache directory; wasmtime refuses one made by another
        // wasmtime or with other settings.
        match unsafe { Module::deserialize_file(engine, &path) } {
            Ok(module) => {
                touch(&path);
                return Ok(module);
            }
            Err(error) => eprintln!("[collabo-core] compiling again ({}: {error})", path.display()),
        }
    }
    let module = compile_large(engine, bytes)?;
    match store(&module, &dir, &path) {
        // the mapped file instead of the compiled copy in memory
        Ok(()) => match unsafe { Module::deserialize_file(engine, &path) } {
            Ok(mapped) => {
                drop(module);
                give_back_freed_memory();
                Ok(mapped)
            }
            Err(_) => Ok(module),
        },
        Err(error) => {
            eprintln!("[collabo-core] could not keep the compiled program in {}: {error}", dir.display());
            Ok(module)
        }
    }
}

fn compile_large(engine: &Engine, bytes: &[u8]) -> Result<Module> {
    let module = Module::new(engine, bytes);
    give_back_freed_memory();
    module
}

/// Compiling leaves gigabytes freed but held by the allocator (cranelift's per-function work);
/// glibc keeps such memory for later allocations unless asked to return it.
fn give_back_freed_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only returns free heap memory to the system.
    unsafe {
        libc::malloc_trim(0);
    }
}

fn cache_dir() -> Option<PathBuf> {
    let dir = match std::env::var_os("COLLABO_PROGRAM_CACHE") {
        Some(value) if value == "off" || value.is_empty() => return None,
        Some(value) => PathBuf::from(value),
        None => {
            let base = std::env::var_os("XDG_CACHE_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("LOCALAPPDATA").filter(|_| cfg!(windows)).map(PathBuf::from))
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
            base.join("collabo-core").join("programs")
        }
    };
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn store(module: &Module, dir: &Path, path: &Path) -> Result<()> {
    let bytes = module.serialize()?;
    make_room(dir, bytes.len() as u64);
    // whole or not at all: other machines may be reading the directory
    let partial = dir.join(format!(".{}.{}.partial", std::process::id(), rand::random::<u32>()));
    std::fs::write(&partial, &bytes)?;
    if let Err(error) = std::fs::rename(&partial, path) {
        let _ = std::fs::remove_file(&partial);
        return Err(error.into());
    }
    Ok(())
}

/// Marks a file used (the budget deletes the least recently used first).
fn touch(path: &Path) {
    if let Ok(file) = std::fs::File::options().append(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// Deletes the least recently used compiled programs until `incoming` more bytes fit the budget.
fn make_room(dir: &Path, incoming: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "cwasm"))
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            Some((meta.modified().ok()?, meta.len(), entry.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum::<u64>() + incoming;
    files.sort();
    for (_, len, path) in files {
        if total <= BUDGET {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}
