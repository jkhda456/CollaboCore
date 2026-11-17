//! collabo-data: the guest's data format tools in one binary, as busybox does it. The image links
//! each tool's name to it (/usr/bin/jq -> collabo-data); run under another name, the first
//! argument names the tool (`collabo-data plutil -p Info.plist`). Each tool is its own crate,
//! with a `main(argv) -> exit status`:
//!
//!   jq       the JSON processor (../jq)
//!   plutil   property lists: XML, binary, JSON, OpenStep (../plutil)
//!   yq       YAML, JSON, XML, ... (../yq)

/// The tools, by the names they answer to.
const APPLETS: &[&str] = &["jq", "plutil", "yq"];

fn usage() -> String {
    format!("usage: collabo-data APPLET [ARGS...]\n       collabo-data --list\napplets: {}", APPLETS.join(" "))
}

fn main() {
    // SIGPIPE: end quietly on a closed pipe, as the C (and Go) originals do.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let mut argv: Vec<String> = std::env::args_os().map(|a| a.to_string_lossy().into_owned()).collect();
    let path = argv.first().cloned().unwrap_or_else(|| "collabo-data".into());
    let base = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    let mut name = base(&path);
    if !APPLETS.contains(&name.as_str()) {
        match argv.get(1).map(String::as_str) {
            Some(applet) if APPLETS.contains(&applet) => {
                // As if run by the applet's link beside this binary: jq finds $ORIGIN from it.
                name = argv.remove(1);
                argv[0] = match path.rfind('/') {
                    Some(at) => format!("{}/{name}", &path[..at]),
                    None => name.clone(),
                };
            }
            Some("--list") => {
                println!("{}", APPLETS.join("\n"));
                std::process::exit(0);
            }
            Some("--help" | "-h") => {
                println!("{}", usage());
                std::process::exit(0);
            }
            _ => {
                eprintln!("{}", usage());
                std::process::exit(1);
            }
        }
    }
    let code = match name.as_str() {
        "jq" => collabo_jq::main(argv),
        "plutil" => collabo_plutil::main(argv),
        "yq" => collabo_yq::main(argv),
        _ => unreachable!(),
    };
    std::process::exit(code);
}
