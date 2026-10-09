//! Links libcurl: on the guest the static libcurl, OpenSSL and zlib the tools build makes
//! (COLLABO_CURL_LIBDIRS: colon-separated library directories); on the host, the system's.
fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rerun-if-env-changed=COLLABO_CURL_LIBDIRS");
    if target.starts_with("wasm32") {
        for d in std::env::var("COLLABO_CURL_LIBDIRS").unwrap_or_default().split(':').filter(|d| !d.is_empty()) {
            println!("cargo:rustc-link-search=native={d}");
        }
        for l in ["curl", "ssl", "crypto", "z"] {
            println!("cargo:rustc-link-lib=static={l}");
        }
    } else {
        // No development symlink needed: the shared library by its soname.
        let candidates = ["/usr/lib/x86_64-linux-gnu/libcurl.so.4", "/usr/lib/aarch64-linux-gnu/libcurl.so.4", "/usr/lib/libcurl.so.4", "/usr/lib64/libcurl.so.4"];
        match candidates.iter().find(|p| std::path::Path::new(p).exists()) {
            Some(p) => println!("cargo:rustc-link-arg={p}"),
            None => println!("cargo:rustc-link-lib=curl"),
        }
    }
}
