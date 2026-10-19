//! `launcher --addons`: the add-ons of a GitHub release, listed and fetched into the add-on folder.
//!
//! The release (by default the latest of jkhda456/CollaboAddons, `addons-repo` in
//! launcher.conf for another) has one `<name>-wasm.zip` per add-on, holding `<name>.cpio`,
//! `<name>.json` and `licenses/*`. `get` always downloads the release's current zip again and
//! puts `<name>.*` in the add-on folder and each `licenses/FILE` in app/licenses as
//! `addon-<name>-FILE`, replacing what was there; there are no versions to choose from.
use std::fs::File;
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The release `addons-repo` replaces.
pub const DEFAULT_REPO: &str = "https://api.github.com/repos/jkhda456/CollaboAddons/releases/latest";

/// Where add-ons come from and go.
pub struct Place {
    /// The release's API URL.
    pub repo: String,
    /// The add-on folder (`<name>.cpio`, `<name>.json`).
    pub dir: PathBuf,
    /// The runtime's licenses folder.
    pub licenses: PathBuf,
}

/// `addons-repo = VALUE`: an API URL as it is, `OWNER/REPO` the latest release there.
pub fn repo_url(value: &str) -> String {
    if value.contains("://") {
        value.to_string()
    } else {
        format!("https://api.github.com/repos/{}/releases/latest", value.trim_matches('/'))
    }
}

/// `launcher --addons [list | get NAME...]`.
pub fn command(args: &[String], place: &Place) -> Result<i32, String> {
    match args.first().map(String::as_str) {
        None | Some("list") if args.len() <= 1 => list(place),
        Some("get") if args.len() > 1 => get(&args[1..], place),
        Some("get") => Err("--addons get needs the name of an add-on (launcher --addons list)".into()),
        _ => Err(format!("--addons takes list or get NAME..., not {}", args.join(" "))),
    }
}

/// One add-on of the release.
#[derive(Debug, PartialEq)]
struct Asset {
    name: String,
    size: u64,
    url: String,
}

/// An add-on's name from its zip's: `-wasm.zip` (or `.zip`) off.
fn addon_name(file: &str) -> Option<&str> {
    let stem = file.strip_suffix(".zip")?;
    Some(stem.strip_suffix("-wasm").unwrap_or(stem)).filter(|name| !name.is_empty())
}

/// The zips of a release (the GitHub API's JSON), by name.
fn assets(release: &serde_json::Value) -> Result<Vec<Asset>, String> {
    let list = release["assets"].as_array().ok_or("the release has no assets list")?;
    let mut assets: Vec<Asset> = list
        .iter()
        .filter_map(|asset| {
            Some(Asset {
                name: addon_name(asset["name"].as_str()?)?.to_string(),
                size: asset["size"].as_u64().unwrap_or(0),
                url: asset["browser_download_url"].as_str()?.to_string(),
            })
        })
        .collect();
    assets.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(assets)
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .user_agent(concat!("collabo-core-launcher/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
            .build(),
    )
}

fn http_error(url: &str, error: ureq::Error) -> String {
    match error {
        ureq::Error::StatusCode(403 | 429) => format!("{url}: refused (GitHub allows 60 requests an hour without a login; try again later)"),
        ureq::Error::StatusCode(404) => format!("{url}: not found (addons-repo in launcher.conf)"),
        error => format!("{url}: {error}"),
    }
}

fn release(repo: &str) -> Result<Vec<Asset>, String> {
    let mut response =
        agent().get(repo).header("accept", "application/vnd.github+json").call().map_err(|e| http_error(repo, e))?;
    let text = response.body_mut().read_to_string().map_err(|e| format!("{repo}: {e}"))?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{repo}: {e}"))?;
    assets(&json).map_err(|e| format!("{repo}: {e}"))
}

/// The add-ons in the folder: its `<name>.cpio` files.
fn installed(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.strip_suffix(".cpio").map(str::to_string))
        .collect();
    names.sort();
    names
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// The release's add-ons, those in the folder marked, then those only in the folder.
fn listing(assets: &[Asset], installed: &[String]) -> String {
    let width = assets.iter().map(|a| a.name.len()).chain(installed.iter().map(String::len)).max().unwrap_or(0);
    let mut out = String::new();
    for asset in assets {
        let mark = if installed.contains(&asset.name) { "  installed" } else { "" };
        out += &format!("  {:width$}  {:>9}{mark}\n", asset.name, megabytes(asset.size));
    }
    let own: Vec<&String> = installed.iter().filter(|name| !assets.iter().any(|a| &a.name == *name)).collect();
    if !own.is_empty() {
        out += "\nOnly in this runtime (not in the repository):\n";
        for name in own {
            out += &format!("  {name}\n");
        }
    }
    out
}

fn list(place: &Place) -> Result<i32, String> {
    let assets = release(&place.repo)?;
    println!("Add-ons in {}", place.repo);
    println!("(installed: {}; launcher --addons get NAME downloads one)\n", place.dir.display());
    print!("{}", listing(&assets, &installed(&place.dir)));
    Ok(0)
}

fn get(names: &[String], place: &Place) -> Result<i32, String> {
    let assets = release(&place.repo)?;
    let mut wanted: Vec<&Asset> = Vec::new();
    for name in names {
        let name = name.strip_suffix(".zip").unwrap_or(name);
        let name = name.strip_suffix("-wasm").unwrap_or(name);
        let asset = assets.iter().find(|a| a.name == name).ok_or_else(|| {
            let known: Vec<&str> = assets.iter().map(|a| a.name.as_str()).collect();
            format!("no add-on {name} in {} (there are: {})", place.repo, known.join(", "))
        })?;
        if !wanted.contains(&asset) {
            wanted.push(asset);
        }
    }
    std::fs::create_dir_all(&place.dir).map_err(|e| format!("making {}: {e}", place.dir.display()))?;
    for asset in wanted {
        let part = place.dir.join(format!(".{}-wasm.zip.part", asset.name));
        let result = download(asset, &part).and_then(|()| install(&part, &asset.name, place));
        let _ = std::fs::remove_file(&part);
        result?;
    }
    Ok(0)
}

fn download(asset: &Asset, to: &Path) -> Result<(), String> {
    let mut response = agent().get(&asset.url).call().map_err(|e| http_error(&asset.url, e))?;
    let total = response.body().content_length().unwrap_or(asset.size);
    let mut body = response.body_mut().as_reader();
    let mut file = File::create(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let progress = std::io::stderr().is_terminal();
    let (mut buffer, mut done, mut shown) = (vec![0u8; 256 * 1024], 0u64, 0u64);
    loop {
        let n = body.read(&mut buffer).map_err(|e| format!("{}: {e}", asset.url))?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n]).map_err(|e| format!("{}: {e}", to.display()))?;
        done += n as u64;
        if progress && (done - shown >= 1 << 20 || done == total) {
            shown = done;
            eprint!("\r{}: {} / {}  ", asset.name, megabytes(done), megabytes(total));
        }
    }
    if progress {
        eprint!("\r\x1b[K");
    }
    eprintln!("{}: downloaded {}", asset.name, megabytes(done));
    Ok(())
}

/// Where a file of `<name>`'s zip goes.
#[derive(Debug, PartialEq)]
enum Target {
    /// In the add-on folder (only `<name>.*`, so a zip cannot replace another add-on).
    Addon(String),
    /// In the licenses folder, as `addon-<name>-FILE`.
    License(String),
}

fn target(entry: &str, name: &str) -> Option<Target> {
    let plain = |part: &str| !part.is_empty() && part != "." && part != ".." && !part.contains(['\\', ':']);
    let parts: Vec<&str> = entry.split('/').collect();
    match parts[..] {
        [file] if plain(file) && file.strip_prefix(name).is_some_and(|rest| rest.starts_with('.')) => {
            Some(Target::Addon(file.to_string()))
        }
        ["licenses", file] if plain(file) => {
            let prefix = format!("addon-{name}-");
            Some(Target::License(if file.starts_with(&prefix) { file.to_string() } else { format!("{prefix}{file}") }))
        }
        _ => None,
    }
}

/// Unpacks the zip: every file to a `.part` beside its place, then, all of them written, each into
/// its place, so a broken download replaces nothing.
fn install(zip: &Path, name: &str, place: &Place) -> Result<(), String> {
    let at = |e: String| format!("{}: {e}", zip.display());
    let mut file = File::open(zip).map_err(|e| at(e.to_string()))?;
    let entries = central_directory(&mut file).map_err(at)?;
    let mut planned = Vec::new();
    for entry in &entries {
        if entry.name.ends_with('/') {
            continue;
        }
        match target(&entry.name, name) {
            Some(Target::Addon(file)) => planned.push((entry, place.dir.join(file))),
            Some(Target::License(file)) => planned.push((entry, place.licenses.join(file))),
            None => eprintln!("{name}: left out {} (not {name}.* or licenses/*)", entry.name),
        }
    }
    let cpio = place.dir.join(format!("{name}.cpio"));
    if !planned.iter().any(|(_, path)| *path == cpio) {
        return Err(at(format!("no {name}.cpio in it")));
    }
    std::fs::create_dir_all(&place.licenses).map_err(|e| format!("making {}: {e}", place.licenses.display()))?;

    let part = |path: &Path| path.with_file_name(format!("{}.part", path.file_name().unwrap().to_string_lossy()));
    let mut written = Vec::new();
    let result = planned.iter().try_for_each(|(entry, path)| {
        written.push(part(path));
        extract(&mut file, entry, &part(path)).map_err(|e| at(format!("{}: {e}", entry.name)))
    });
    if let Err(error) = result {
        for path in written {
            let _ = std::fs::remove_file(path);
        }
        return Err(error);
    }

    // The licenses of the add-on's previous version go: the new ones replace them.
    let prefix = format!("addon-{name}-");
    for old in std::fs::read_dir(&place.licenses).into_iter().flatten().flatten() {
        let file_name = old.file_name().to_string_lossy().into_owned();
        if file_name.starts_with(&prefix) && !file_name.ends_with(".part") {
            let _ = std::fs::remove_file(old.path());
        }
    }
    for (_, path) in &planned {
        std::fs::rename(part(path), path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let licenses = planned.iter().filter(|(_, path)| path.starts_with(&place.licenses)).count();
    eprintln!("{name}: installed in {} ({licenses} license files); launcher --addon {name} boots with it", place.dir.display());
    Ok(())
}

/// A file in a zip, as its central directory describes it.
#[derive(Debug)]
struct Entry {
    name: String,
    method: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    offset: u64,
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn central_directory(file: &mut (impl Read + Seek)) -> Result<Vec<Entry>, String> {
    let io = |e: std::io::Error| e.to_string();
    let len = file.seek(SeekFrom::End(0)).map_err(io)?;
    // The end record is 22 bytes and a comment of at most 65535.
    let tail_len = len.min(22 + 65535);
    let mut tail = vec![0u8; tail_len as usize];
    file.seek(SeekFrom::Start(len - tail_len)).map_err(io)?;
    file.read_exact(&mut tail).map_err(io)?;
    let end = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| u32_at(&tail, i) == 0x0605_4b50)
        .ok_or("not a zip archive")?;
    let (count, size, start) = (u16_at(&tail, end + 10), u32_at(&tail, end + 12), u32_at(&tail, end + 16));
    if count == 0xffff || size == u32::MAX || start == u32::MAX {
        return Err("a zip64 archive (not supported)".into());
    }
    let mut directory = vec![0u8; size as usize];
    file.seek(SeekFrom::Start(start as u64)).map_err(io)?;
    file.read_exact(&mut directory).map_err(io)?;

    let mut entries = Vec::new();
    let mut at = 0;
    for _ in 0..count {
        if at + 46 > directory.len() || u32_at(&directory, at) != 0x0201_4b50 {
            return Err("a broken central directory".into());
        }
        let (name_len, extra_len, comment_len) =
            (u16_at(&directory, at + 28) as usize, u16_at(&directory, at + 30) as usize, u16_at(&directory, at + 32) as usize);
        let name = directory.get(at + 46..at + 46 + name_len).ok_or("a broken central directory")?;
        if u16_at(&directory, at + 8) & 1 != 0 {
            return Err(format!("{} is encrypted", String::from_utf8_lossy(name)));
        }
        entries.push(Entry {
            name: String::from_utf8_lossy(name).into_owned(),
            method: u16_at(&directory, at + 10),
            crc: u32_at(&directory, at + 16),
            compressed: u32_at(&directory, at + 20) as u64,
            size: u32_at(&directory, at + 24) as u64,
            offset: u32_at(&directory, at + 42) as u64,
        });
        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

/// Writes one entry's bytes to `to`, checking their size and CRC.
fn extract(file: &mut File, entry: &Entry, to: &Path) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    let mut local = [0u8; 30];
    file.seek(SeekFrom::Start(entry.offset)).map_err(io)?;
    file.read_exact(&mut local).map_err(io)?;
    if u32_at(&local, 0) != 0x0403_4b50 {
        return Err("a broken local header".into());
    }
    let skip = u16_at(&local, 26) as i64 + u16_at(&local, 28) as i64;
    file.seek(SeekFrom::Current(skip)).map_err(io)?;
    let data = (&mut *file).take(entry.compressed);
    let mut reader: Box<dyn Read + '_> = match entry.method {
        0 => Box::new(data),
        8 => Box::new(flate2::read::DeflateDecoder::new(data)),
        method => return Err(format!("compression method {method} (not supported)")),
    };
    let mut out = std::io::BufWriter::new(File::create(to).map_err(|e| format!("{}: {e}", to.display()))?);
    let (mut buffer, mut crc, mut size) = (vec![0u8; 256 * 1024], crc32fast::Hasher::new(), 0u64);
    loop {
        let n = reader.read(&mut buffer).map_err(io)?;
        if n == 0 {
            break;
        }
        crc.update(&buffer[..n]);
        size += n as u64;
        out.write_all(&buffer[..n]).map_err(|e| format!("{}: {e}", to.display()))?;
    }
    out.flush().map_err(|e| format!("{}: {e}", to.display()))?;
    if size != entry.size || crc.finalize() != entry.crc {
        return Err("its size or CRC does not match (a broken download?)".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn release_assets_are_add_ons_by_name() {
        let release = serde_json::json!({"assets": [
            {"name": "tcpdump-wasm.zip", "size": 482379, "browser_download_url": "https://x/tcpdump-wasm.zip"},
            {"name": "agent-wasm.zip", "size": 666826, "browser_download_url": "https://x/agent-wasm.zip"},
            {"name": "c-sh-wasm.zip", "size": 873251, "browser_download_url": "https://x/c-sh-wasm.zip"},
            {"name": "SHA256SUMS", "size": 10, "browser_download_url": "https://x/SHA256SUMS"},
        ]});
        let assets = assets(&release).unwrap();
        assert_eq!(assets.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["agent", "c-sh", "tcpdump"]);
        assert_eq!(assets[2], Asset { name: "tcpdump".into(), size: 482379, url: "https://x/tcpdump-wasm.zip".into() });
        assert_eq!(addon_name("plain.zip"), Some("plain"));
        assert_eq!(addon_name("-wasm.zip"), None);
        assert!(super::assets(&serde_json::json!({"message": "Not Found"})).is_err());
    }

    #[test]
    fn listing_marks_installed_and_own_add_ons() {
        let asset = |name: &str| Asset { name: name.into(), size: 1_500_000, url: String::new() };
        let text = listing(&[asset("nmap"), asset("node")], &["mine".into(), "node".into()]);
        assert_eq!(text, "  nmap     1.5 MB\n  node     1.5 MB  installed\n\nOnly in this runtime (not in the repository):\n  mine\n");
    }

    #[test]
    fn repo_takes_a_url_or_owner_and_repo() {
        assert_eq!(repo_url("me/addons"), "https://api.github.com/repos/me/addons/releases/latest");
        assert_eq!(repo_url("https://h/api/releases/tags/v1"), "https://h/api/releases/tags/v1");
    }

    #[test]
    fn zip_files_go_to_their_add_on_only() {
        assert_eq!(target("nmap.cpio", "nmap"), Some(Target::Addon("nmap.cpio".into())));
        assert_eq!(target("nmap.json", "nmap"), Some(Target::Addon("nmap.json".into())));
        assert_eq!(target("licenses/nmap.txt", "nmap"), Some(Target::License("addon-nmap-nmap.txt".into())));
        assert_eq!(target("licenses/addon-nmap-x.txt", "nmap"), Some(Target::License("addon-nmap-x.txt".into())));
        for other in ["node.cpio", "nmapx.cpio", "nmap", "../nmap.cpio", "licenses/../x", "licenses/a/b", "x/nmap.cpio", "licenses/a\\b"] {
            assert_eq!(target(other, "nmap"), None, "{other}");
        }
    }

    #[test]
    fn a_stored_zip_unpacks() {
        let dir = std::env::temp_dir().join(format!("launcher-addons-{}", std::process::id()));
        let source = dir.join("src");
        std::fs::create_dir_all(source.join("licenses")).unwrap();
        std::fs::write(source.join("demo.cpio"), b"070701 cpio").unwrap();
        std::fs::write(source.join("demo.json"), b"{}").unwrap();
        std::fs::write(source.join("other.cpio"), b"not mine").unwrap();
        std::fs::write(source.join("licenses/demo.txt"), b"MIT").unwrap();
        let zip = dir.join("demo-wasm.zip");
        let mut command = Command::new("zip");
        if !command.args(["-qr", zip.to_str().unwrap(), "."]).current_dir(&source).status().is_ok_and(|s| s.success()) {
            eprintln!("no zip command; skipped");
            return;
        }
        let place = Place { repo: String::new(), dir: dir.join("addons"), licenses: dir.join("licenses") };
        std::fs::create_dir_all(&place.dir).unwrap();
        std::fs::create_dir_all(&place.licenses).unwrap();
        std::fs::write(place.licenses.join("addon-demo-stale.txt"), b"old").unwrap();
        install(&zip, "demo", &place).unwrap();
        assert_eq!(std::fs::read(place.dir.join("demo.cpio")).unwrap(), b"070701 cpio");
        assert_eq!(std::fs::read(place.dir.join("demo.json")).unwrap(), b"{}");
        assert!(!place.dir.join("other.cpio").exists());
        assert_eq!(std::fs::read(place.licenses.join("addon-demo-demo.txt")).unwrap(), b"MIT");
        assert!(!place.licenses.join("addon-demo-stale.txt").exists());
        assert!(install(&zip, "nothere", &place).unwrap_err().contains("no nothere.cpio"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
