//! collabo-archive: xz, zstd and 7z for the collaboCore guest in one binary, which acts as the
//! name it is run as (the image links each name to it), or as its first argument's
//! (`collabo-archive xz -d f.xz`).

mod common;
mod getopt;
mod sevenz;
mod verify;
mod xz;
mod zstd;
mod xzindex;

const APPLETS: &[&str] = &["xz", "unxz", "xzcat", "lzma", "unlzma", "lzcat", "zstd", "unzstd", "zstdcat", "zstdmt", "7z", "7za", "7zr", "7zz"];

fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    let mut name = std::path::Path::new(args.first().map(String::as_str).unwrap_or("xz"))
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    args.remove(0);
    if !APPLETS.contains(&name.as_str()) {
        match args.first() {
            Some(a) if APPLETS.contains(&a.as_str()) => name = args.remove(0),
            _ => {
                eprintln!("usage: collabo-archive APPLET [ARGS...]\napplets: {}", APPLETS.join(" "));
                std::process::exit(1);
            }
        }
    }
    *common::PROG.lock().unwrap() = name.clone();
    let code = match name.as_str() {
        "xz" | "unxz" | "xzcat" | "lzma" | "unlzma" | "lzcat" => xz::main(&name, args),
        "zstd" | "unzstd" | "zstdcat" | "zstdmt" => zstd::main(&name, args),
        "7z" | "7za" | "7zr" | "7zz" => sevenz::main(&name, args),
        _ => unreachable!(),
    };
    std::process::exit(code);
}
