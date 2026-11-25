//! `launcher --update`: this runtime from a newer collaboCore release on GitHub.
//!
//! The release (by default the latest of jkhda456/CollaboCore; `update-repo` in launcher.conf
//! for another) has one archive per platform, `collabo-core-<platform>.tar.gz` (`.zip` for
//! Windows), holding the runtime folder `collabo-core-<platform>/`. `--update` downloads this
//! platform's into the current folder when the release is newer than this launcher;
//! `--update --apply` puts one in place of this runtime: the newest that an earlier `--update`
//! downloaded (in the current folder or the runtime's) when it is newer than this runtime, else
//! the release's, downloaded then; `--update --apply FILE` that archive.
//!
//! - It is unpacked in `.update/` in the runtime folder before anything else changes; a broken
//!   download fails there (the zip's CRCs, gzip's, the size GitHub gives for a download). Its
//!   manifest.json must list the launcher and the engine, and every file it lists must be in the
//!   archive; the sha256 sums it gives are not compared: releases put together from several
//!   machines' builds have held newer images than their manifests list (0.2.17's).
//! - Each file of the new manifest, then manifest.json, takes the place of the old one, which is
//!   moved into `.update/` first; the old manifest's files the new one no longer has go there
//!   too. Moves, not copies or deletes: a program that is running (this launcher, the engine of a
//!   sandbox still open) can be renamed on Windows but not overwritten or deleted, and inside the
//!   runtime folder a move never crosses to another drive or file system (as one to /tmp or
//!   %TEMP% may, where a rename fails).
//! - launcher.conf is the user's: the archive's is not even unpacked, so the runtime's stays as
//!   it is (or missing, when it was). What no manifest lists (work/, logs, the add-ons from
//!   `--addons get`) stays as it is too.
//! - The new launcher must start with this runtime's manifest and launcher.conf (`launcher
//!   --dry-run`: an older one, say, may not know a key the conf has), or every move is undone.
//!   Then `.update/` is removed; a file Windows keeps open (the launcher that ran the update)
//!   stays in it until the next `--update` clears it.
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::platform::{PLATFORM, VERSION};

/// The release `update-repo` replaces.
pub const DEFAULT_REPO: &str = "https://api.github.com/repos/jkhda456/CollaboCore/releases/latest";

/// The folder in the runtime that an update is unpacked in and the old files are moved to.
const WORK: &str = ".update";
const LAUNCHER: &str = if cfg!(windows) { "launcher.exe" } else { "launcher" };
const CONF: &str = "launcher.conf";
const MANIFEST: &str = "manifest.json";

/// Where the update comes from and goes.
pub struct Place {
    /// The release's API URL.
    pub repo: String,
    /// The runtime folder (the launcher's).
    pub base: PathBuf,
    /// launcher.conf keeps the engine or the images somewhere else (`bin`, `app`).
    pub moved: bool,
}

/// What `--update` was asked to do.
#[derive(Debug, Default, PartialEq)]
struct Request {
    check: bool,
    apply: bool,
    force: bool,
    archive: Option<PathBuf>,
}

fn request(args: &[String]) -> Result<Request, String> {
    let mut request = Request::default();
    for arg in args {
        match arg.as_str() {
            "--check" => request.check = true,
            "--apply" => request.apply = true,
            "--force" => request.force = true,
            option if option.starts_with("--") => {
                return Err(format!("--update takes --check, --apply, --force and an archive, not {option}"))
            }
            file if request.archive.is_none() => request.archive = Some(PathBuf::from(file)),
            extra => return Err(format!("--update takes one archive, not also {extra}")),
        }
    }
    if request.check && (request.apply || request.force || request.archive.is_some()) {
        return Err("--update --check only looks at the release (no --apply, --force or archive with it)".into());
    }
    if request.archive.is_some() && !request.apply {
        return Err("an archive is for --apply: launcher --update --apply FILE".into());
    }
    Ok(request)
}

/// `launcher --update [--check | --apply [FILE]] [--force]`.
pub fn command(args: &[String], place: &Place) -> Result<i32, String> {
    let request = request(args)?;
    if request.apply && place.moved {
        return Err("launcher.conf keeps the engine or the images elsewhere (bin, app); update them where they are".into());
    }
    let work = place.base.join(WORK);
    if request.apply {
        clear(&work);
    }
    if let Some(archive) = &request.archive {
        let archive = archive.canonicalize().map_err(|e| format!("{}: {e}", archive.display()))?;
        return apply(&archive, &place.base, None, &runs).map(|()| 0);
    }
    if request.apply {
        let here = std::env::current_dir().map_err(|e| format!("the current folder: {e}"))?;
        if let Some((archive, version)) = downloaded(&[here, place.base.clone()], request.force) {
            println!("{} (downloaded before): collaboCore {version}, this runtime {VERSION}", archive.display());
            return apply(&archive, &place.base, Some(&version), &runs).map(|()| 0);
        }
    }

    let release = latest(&place.repo)?;
    let order = compare(&release.tag, VERSION);
    match order {
        Ordering::Greater => println!("collaboCore {} is out (this runtime: {VERSION}, {PLATFORM})", release.tag),
        Ordering::Equal => println!("this runtime is up to date: {VERSION} ({PLATFORM}) is the latest release"),
        Ordering::Less => println!("this runtime ({VERSION}) is newer than the latest release ({})", release.tag),
    }
    if request.check {
        return Ok(0);
    }
    if order != Ordering::Greater && !request.force {
        println!("(--force {} all the same)", if request.apply { "puts it in place" } else { "downloads it" });
        return Ok(0);
    }
    if request.apply {
        std::fs::create_dir_all(&work).map_err(|e| format!("making {}: {e}", work.display()))?;
        let file = work.join(format!("{}-{}", std::process::id(), release.name));
        let result = fetch(&release, &file).and_then(|()| apply(&file, &place.base, Some(&release.tag), &runs));
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&work);
        return result.map(|()| 0);
    }
    let here = std::env::current_dir().map_err(|e| format!("the current folder: {e}"))?;
    let to = here.join(download_name(&release));
    fetch(&release, &to)?;
    println!("{}\n`launcher --update --apply` (from this folder or the runtime's) puts it in place of this runtime", to.display());
    Ok(0)
}

/// The version in the name of an archive `--update` downloaded for this platform
/// (download_name), when it is one.
fn downloaded_version(name: &str) -> Option<&str> {
    let asset = archive_name(PLATFORM);
    let ext = if asset.ends_with(".tar.gz") { ".tar.gz" } else { ".zip" };
    let stem = &asset[..asset.len() - ext.len()];
    let version = name.strip_prefix(stem)?.strip_prefix('-')?.strip_suffix(ext)?;
    version_key(version).map(|_| version)
}

/// The newest archive an earlier `--update` downloaded into one of `dirs`, and its version:
/// one newer than this runtime, or with --force any.
fn downloaded(dirs: &[PathBuf], force: bool) -> Option<(PathBuf, String)> {
    let mut best: Option<(PathBuf, String)> = None;
    for dir in dirs {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(version) = downloaded_version(&name) else { continue };
            if !entry.path().is_file() || (!force && compare(version, VERSION) != Ordering::Greater) {
                continue;
            }
            if best.as_ref().map_or(true, |(_, newest)| compare(version, newest) == Ordering::Greater) {
                best = Some((entry.path(), version.to_string()));
            }
        }
    }
    best
}

/// This platform's archive in a release.
#[derive(Debug, PartialEq)]
struct Release {
    tag: String,
    name: String,
    size: u64,
    url: String,
}

fn archive_name(platform: &str) -> String {
    format!("collabo-core-{platform}{}", if platform.starts_with("win-") { ".zip" } else { ".tar.gz" })
}

fn release_asset(release: &serde_json::Value, platform: &str) -> Result<Release, String> {
    let tag = release["tag_name"].as_str().ok_or("the release has no tag")?;
    let assets = release["assets"].as_array().ok_or("the release has no assets list")?;
    let name = archive_name(platform);
    let asset = assets.iter().find(|asset| asset["name"].as_str() == Some(&name)).ok_or_else(|| {
        let there: Vec<&str> = assets.iter().filter_map(|asset| asset["name"].as_str()).collect();
        format!("the release {tag} has no {name} (it has: {})", there.join(", "))
    })?;
    Ok(Release {
        tag: tag.to_string(),
        size: asset["size"].as_u64().unwrap_or(0),
        url: asset["browser_download_url"].as_str().ok_or_else(|| format!("{name} has no download URL"))?.to_string(),
        name,
    })
}

fn latest(repo: &str) -> Result<Release, String> {
    release_asset(&crate::fetch::release(repo, "update-repo")?, PLATFORM).map_err(|e| format!("{repo}: {e}"))
}

/// The downloaded archive's name: the asset's with the version in it.
fn download_name(release: &Release) -> String {
    let tag: String =
        release.tag.chars().map(|c| if c.is_ascii_alphanumeric() || ".-_".contains(c) { c } else { '_' }).collect();
    let (stem, ext) = release.name.split_at(release.name.find(".tar.gz").or_else(|| release.name.rfind('.')).unwrap_or(release.name.len()));
    format!("{stem}-{tag}{ext}")
}

/// Downloads through a `.part` file, so a broken download is never taken for the archive.
fn fetch(release: &Release, to: &Path) -> Result<(), String> {
    let part = to.with_file_name(format!("{}.part", to.file_name().unwrap_or_default().to_string_lossy()));
    let result = crate::fetch::download(&release.url, &release.name, release.size, &part)
        .and_then(|()| std::fs::rename(&part, to).map_err(|e| format!("{}: {e}", to.display())));
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

/// A version's numbers and its pre-release part (`0.3.0-rc1`); a leading v is dropped.
fn version_key(version: &str) -> Option<(Vec<u64>, Option<&str>)> {
    let version = version.trim().trim_start_matches(['v', 'V']);
    let (numbers, pre) = match version.split_once('-') {
        Some((numbers, pre)) => (numbers, Some(pre)),
        None => (version, None),
    };
    Some((numbers.split('.').map(|n| n.parse().ok()).collect::<Option<Vec<u64>>>()?, pre))
}

/// How the release's tag stands to this version. A tag that is no version counts as newer
/// unless it is this one.
fn compare(tag: &str, current: &str) -> Ordering {
    let (Some((mut a, a_pre)), Some((mut b, b_pre))) = (version_key(tag), version_key(current)) else {
        return if tag.trim_start_matches(['v', 'V']) == current { Ordering::Equal } else { Ordering::Greater };
    };
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    a.cmp(&b).then_with(|| match (a_pre, b_pre) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => x.cmp(y),
    })
}

/// A path inside the runtime folder, `/`-separated, that cannot leave it.
fn safe(rel: &str) -> Result<(), String> {
    let bad = rel.is_empty()
        || rel.split('/').any(|part| part.is_empty() || part == "." || part == ".." || part.contains(['\\', ':', '\0']));
    if bad {
        return Err(format!("{rel:?}: not a path inside the runtime"));
    }
    Ok(())
}

fn join(base: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(base.to_path_buf(), |path, part| path.join(part))
}

/// A file's sha256 (the tests' manifests).
#[cfg(test)]
fn sha256(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        context.update(&buffer[..n]);
    }
    Ok(context.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect())
}

/// Where an archive's entries go: under `to`, without the archive's one top folder
/// (`collabo-core-<platform>/`).
struct Unpacker<'a> {
    to: &'a Path,
    top: Option<String>,
    files: BTreeSet<String>,
}

impl Unpacker<'_> {
    /// The place of an entry (a zip made on Windows may use `\`), or None for the top folder
    /// and for launcher.conf, which an update leaves alone.
    fn place(&mut self, name: &str) -> Result<Option<PathBuf>, String> {
        let name = name.replace('\\', "/");
        let name = name.trim_start_matches("./").trim_end_matches('/');
        let (top, rel) = name.split_once('/').unwrap_or((name, ""));
        if !top.starts_with("collabo-core-") {
            return Err(format!("{name}: not in a collabo-core-<platform> folder (not a collaboCore runtime?)"));
        }
        match &self.top {
            Some(known) if known != top => return Err(format!("{name}: a second folder beside {known}")),
            Some(_) => {}
            None => self.top = Some(top.to_string()),
        }
        if rel.is_empty() {
            return Ok(None);
        }
        safe(rel)?;
        if rel == CONF {
            return Ok(None);
        }
        let path = join(self.to, rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("making {}: {e}", parent.display()))?;
        }
        self.files.insert(rel.to_string());
        Ok(Some(path))
    }
}

/// Unpacks a .tar.gz or .zip (by its first bytes) into `to`: the files, by their paths in the
/// runtime. (apply() names the archive in an error.)
fn unpack(archive: &Path, to: &Path) -> Result<BTreeSet<String>, String> {
    let mut file = File::open(archive).map_err(|e| e.to_string())?;
    let mut magic = [0u8; 4];
    let n = file.read(&mut magic).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut unpacker = Unpacker { to, top: None, files: BTreeSet::new() };
    if n >= 2 && magic[..2] == [0x1f, 0x8b] {
        crate::archive::tar_gz_files(file, &mut |entry, body| {
            let Some(path) = unpacker.place(&entry.name)? else { return Ok(()) };
            let mut out = File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            std::io::copy(body, &mut out).map_err(|e| format!("{}: {e}", entry.name))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if entry.mode != 0 {
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(entry.mode));
                }
            }
            Ok(())
        })?;
    } else if n == 4 && magic == *b"PK\x03\x04" {
        for entry in crate::archive::central_directory(&mut file)? {
            if entry.name.ends_with(['/', '\\']) {
                continue;
            }
            if let Some(path) = unpacker.place(&entry.name)? {
                crate::archive::extract(&mut file, &entry, &path).map_err(|e| format!("{}: {e}", entry.name))?;
            }
        }
    } else {
        return Err("not a .tar.gz or a .zip".into());
    }
    Ok(unpacker.files)
}

/// What an unpacked runtime changes.
#[derive(Debug, Default)]
struct Staged {
    /// The new manifest's files, then manifest.json.
    install: Vec<String>,
    /// The old manifest's files the new one does not have.
    remove: Vec<String>,
    /// The new files that differ from the old ones (or are new).
    changed: usize,
}

/// A manifest's `files`: path → sha256.
fn manifest_files(manifest: &serde_json::Value) -> Option<Vec<(String, String)>> {
    let files = manifest["files"].as_object()?;
    Some(files.iter().map(|(rel, sum)| (rel.clone(), sum.as_str().unwrap_or("").to_string())).collect())
}

/// Unpacks the archive into `new` and checks it against its manifest.
fn stage(archive: &Path, new: &Path, base: &Path) -> Result<Staged, String> {
    std::fs::create_dir_all(new).map_err(|e| format!("making {}: {e}", new.display()))?;
    let unpacked = unpack(archive, new)?;
    let text = std::fs::read_to_string(join(new, MANIFEST)).map_err(|_| "it has no manifest.json (not a collaboCore runtime?)")?;
    let manifest: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("its manifest.json: {e}"))?;
    let platform = manifest["platform"].as_str().unwrap_or("?");
    if platform != PLATFORM {
        return Err(format!("it is the runtime for {platform}, and this one is {PLATFORM}"));
    }
    // launcher.conf is the user's whatever a manifest says (it is not unpacked).
    let files: Vec<(String, String)> =
        manifest_files(&manifest).ok_or("its manifest.json has no file list")?.into_iter().filter(|(rel, _)| rel != CONF).collect();
    // Only that the files are there: the sums are not compared (module comment).
    for (rel, _) in &files {
        safe(rel).map_err(|e| format!("its manifest.json: {e}"))?;
        if !join(new, rel).is_file() {
            return Err(format!("{rel}: its manifest.json lists it, and the archive does not have it"));
        }
    }
    let engine = manifest["entry"][0].as_str().unwrap_or("");
    for needed in [LAUNCHER, engine] {
        if !files.iter().any(|(rel, _)| rel == needed) {
            return Err(format!("its manifest.json lists no {}", if needed.is_empty() { "engine" } else { needed }));
        }
    }
    #[cfg(unix)]
    for program in [LAUNCHER, engine] {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(join(new, program), std::fs::Permissions::from_mode(0o755));
    }

    let old: Vec<(String, String)> = std::fs::read_to_string(base.join(MANIFEST))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .and_then(|manifest| manifest_files(&manifest))
        .unwrap_or_default()
        .into_iter()
        .filter(|(rel, _)| safe(rel).is_ok())
        .collect();
    let listed: BTreeSet<String> = files.iter().map(|(rel, _)| rel.clone()).collect();
    for left_out in unpacked.iter().filter(|rel| !listed.contains(*rel) && *rel != MANIFEST) {
        eprintln!("update: left out {left_out} (its manifest.json does not list it)");
    }
    let changed = files.iter().filter(|file| !old.contains(file)).count();
    let mut install: Vec<String> = files.into_iter().map(|(rel, _)| rel).collect();
    install.push(MANIFEST.into());
    let remove = old.into_iter().map(|(rel, _)| rel).filter(|rel| !listed.contains(rel) && rel != CONF).collect();
    Ok(Staged { install, remove, changed })
}

/// The renames made, to undo in reverse.
#[derive(Default)]
struct Moves(Vec<(PathBuf, PathBuf)>);

impl Moves {
    fn rename(&mut self, from: &Path, to: &Path) -> Result<(), String> {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("making {}: {e}", parent.display()))?;
        }
        std::fs::rename(from, to).map_err(|e| format!("moving {} to {}: {e}", from.display(), to.display()))?;
        self.0.push((from.to_path_buf(), to.to_path_buf()));
        Ok(())
    }

    /// Moves `path` into `old` (at `rel`) when it is there.
    fn aside(&mut self, path: &Path, old: &Path, rel: &str) -> Result<(), String> {
        if path.symlink_metadata().is_ok() {
            self.rename(path, &join(old, rel))?;
        }
        Ok(())
    }

    /// Puts everything back; what could not be.
    fn undo(&mut self) -> Vec<String> {
        let mut failed = Vec::new();
        while let Some((from, to)) = self.0.pop() {
            if let Err(e) = std::fs::rename(&to, &from) {
                failed.push(format!("{} (now {}): {e}", from.display(), to.display()));
            }
        }
        failed
    }
}

/// Puts the staged files in place, the old ones into `old`.
fn swap(base: &Path, new: &Path, old: &Path, staged: &Staged, moves: &mut Moves) -> Result<(), String> {
    for rel in &staged.install {
        let path = join(base, rel);
        moves.aside(&path, old, rel)?;
        moves.rename(&join(new, rel), &path)?;
    }
    for rel in &staged.remove {
        moves.aside(&join(base, rel), old, rel)?;
    }
    Ok(())
}

/// Whether the launcher at `path` runs on this machine and takes the runtime beside it, its
/// manifest and launcher.conf (`--dry-run` reads them and starts nothing).
fn runs(path: &Path) -> Result<(), String> {
    let output = Command::new(path).arg("--dry-run").output().map_err(|e| format!("{}: {e}", path.display()))?;
    if output.status.success() && !output.stdout.is_empty() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    Err(format!("launcher --dry-run: {}; {}", output.status, said.trim()))
}

/// Puts the runtime in `archive` in place of the one in `base` (module comment). `tag`: the
/// release's version, for the message; `runs`: whether a launcher runs.
fn apply(archive: &Path, base: &Path, tag: Option<&str>, runs: &dyn Fn(&Path) -> Result<(), String>) -> Result<(), String> {
    let work = base.join(WORK);
    let id = std::process::id();
    let (new, old) = (work.join(format!("new-{id}")), work.join(format!("old-{id}")));
    let _ = std::fs::remove_dir_all(&new);
    let _ = std::fs::remove_dir_all(&old);
    let staged = stage(archive, &new, base);
    let mut moves = Moves::default();
    let outcome = staged.and_then(|staged| {
        let swapped = swap(base, &new, &old, &staged, &mut moves)
            .and_then(|()| runs(&base.join(LAUNCHER)).map_err(|e| format!("the new launcher does not start with this runtime: {e}")));
        match swapped {
            Ok(()) => Ok(staged),
            Err(error) => {
                let failed = moves.undo();
                if failed.is_empty() {
                    Err(format!("{error}\nthis runtime is as it was"))
                } else {
                    Err(format!("{error}\nand these could not be put back (they are in {}):\n  {}", old.display(), failed.join("\n  ")))
                }
            }
        }
    });
    let _ = std::fs::remove_dir_all(&new);
    // Kept when something of this runtime could not be put back.
    let keep_old = outcome.as_ref().is_err_and(|e| e.contains("could not be put back"));
    if !keep_old {
        let _ = std::fs::remove_dir_all(&old);
    }
    let _ = std::fs::remove_dir(&work);
    let staged = outcome.map_err(|e| format!("{}: {e}", archive.display()))?;

    let what = match tag {
        Some(tag) => format!("updated this runtime to {tag} ({PLATFORM})"),
        None => format!("this runtime is now the one in {}", archive.display()),
    };
    let removed = if staged.remove.is_empty() { String::new() } else { format!(", {} no longer in it removed", staged.remove.len()) };
    println!("{what}: {} of {} files changed{removed} ({CONF} left as it is)", staged.changed, staged.install.len() - 1);
    if old.exists() {
        println!("(left in {}: what this system keeps open while it runs; the next --update removes it)", old.display());
    }
    Ok(())
}

/// Removes what earlier updates left in `.update/`, as far as it can.
fn clear(work: &Path) {
    for entry in std::fs::read_dir(work).into_iter().flatten().flatten() {
        let path = entry.path();
        let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENGINE: &str = if cfg!(windows) { "bin/collabo-core-engine.exe" } else { "bin/collabo-core-engine" };

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn versions_compare_by_their_numbers() {
        assert_eq!(compare("0.2.16", "0.2.15"), Ordering::Greater);
        assert_eq!(compare("0.2.15", "0.2.15"), Ordering::Equal);
        assert_eq!(compare("v0.2.15", "0.2.15"), Ordering::Equal);
        assert_eq!(compare("0.2.9", "0.2.15"), Ordering::Less);
        assert_eq!(compare("0.10.0", "0.9.9"), Ordering::Greater);
        assert_eq!(compare("0.3", "0.3.0"), Ordering::Equal);
        assert_eq!(compare("0.3.0-rc1", "0.3.0"), Ordering::Less);
        assert_eq!(compare("0.3.0", "0.3.0-rc1"), Ordering::Greater);
        assert_eq!(compare("nightly", "0.2.15"), Ordering::Greater);
    }

    #[test]
    fn the_release_archive_is_this_platforms() {
        let release = serde_json::json!({"tag_name": "0.2.16", "assets": [
            {"name": "collabo-core-darwin-arm64.tar.gz", "size": 1, "browser_download_url": "https://x/d"},
            {"name": "collabo-core-linux-x64.tar.gz", "size": 60277837, "browser_download_url": "https://x/l"},
            {"name": "collabo-core-win-x64.zip", "size": 3, "browser_download_url": "https://x/w"},
        ]});
        let linux = release_asset(&release, "linux-x64").unwrap();
        assert_eq!(linux, Release { tag: "0.2.16".into(), name: "collabo-core-linux-x64.tar.gz".into(), size: 60277837, url: "https://x/l".into() });
        assert_eq!(download_name(&linux), "collabo-core-linux-x64-0.2.16.tar.gz");
        let windows = release_asset(&release, "win-x64").unwrap();
        assert_eq!(download_name(&windows), "collabo-core-win-x64-0.2.16.zip");
        let missing = release_asset(&release, "win-arm64").unwrap_err();
        assert!(missing.contains("no collabo-core-win-arm64.zip") && missing.contains("collabo-core-win-x64.zip"), "{missing}");
        assert!(release_asset(&serde_json::json!({"message": "Not Found"}), "linux-x64").is_err());
    }

    #[test]
    fn update_arguments() {
        assert_eq!(request(&[]).unwrap(), Request::default());
        assert_eq!(request(&args(&["--apply", "--force"])).unwrap(), Request { apply: true, force: true, ..Request::default() });
        assert_eq!(request(&args(&["--apply", "a.zip"])).unwrap(), Request { apply: true, archive: Some("a.zip".into()), ..Request::default() });
        assert!(request(&args(&["a.zip"])).unwrap_err().contains("--apply FILE"));
        assert!(request(&args(&["--check", "--apply"])).is_err());
        assert!(request(&args(&["--apply", "a", "b"])).unwrap_err().contains("one archive"));
        assert!(request(&args(&["--now"])).unwrap_err().contains("not --now"));
    }

    #[test]
    fn paths_stay_inside_the_runtime() {
        assert!(safe("app/images/addons/agent.cpio").is_ok());
        for bad in ["", "/etc/passwd", "../x", "a/../../x", "a//b", "a/./b", "a\\b", "C:x", "a/"] {
            assert!(safe(bad).is_err(), "{bad}");
        }
        let to = Path::new("/nowhere");
        let mut unpacker = Unpacker { to, top: None, files: BTreeSet::new() };
        assert_eq!(unpacker.place("collabo-core-win-x64/").unwrap(), None);
        assert_eq!(unpacker.place("collabo-core-win-x64/launcher.conf").unwrap(), None);
        assert!(unpacker.files.is_empty());
        assert!(unpacker.place("other/x").unwrap_err().contains("not in a collabo-core-<platform> folder"));
        assert!(unpacker.place("collabo-core-linux-x64/x").unwrap_err().contains("a second folder"));
        assert!(unpacker.place("collabo-core-win-x64/../x").is_err());
    }

    /// A runtime folder: the files, and a manifest.json listing them (`unlisted` not).
    fn runtime(dir: &Path, files: &[(&str, &str)], unlisted: &[(&str, &str)]) {
        let mut sums = serde_json::Map::new();
        for (rel, text) in files.iter().chain(unlisted) {
            let path = join(dir, rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
        }
        for (rel, _) in files {
            sums.insert(rel.to_string(), sha256(&join(dir, rel)).unwrap().into());
        }
        let manifest = serde_json::json!({"platform": PLATFORM, "entry": [ENGINE, "--kernel", "app/images/k"], "files": sums});
        std::fs::write(dir.join(MANIFEST), manifest.to_string()).unwrap();
    }

    fn read(dir: &Path, rel: &str) -> Option<String> {
        std::fs::read_to_string(join(dir, rel)).ok()
    }

    /// An installed runtime and a release archive of a newer one (`change` changes its folder
    /// before it is packed), in a fresh folder; None without a tar command.
    fn scene(name: &str, change: &dyn Fn(&Path)) -> Option<(PathBuf, PathBuf, PathBuf)> {
        let dir = std::env::temp_dir().join(format!("launcher-update-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let base = dir.join("rt");
        let long = format!("app/licenses/{}.txt", "x".repeat(120));
        runtime(
            &base,
            &[(LAUNCHER, "old launcher"), (ENGINE, "old engine"), ("app/images/k", "kernel"), ("app/licenses/gone.txt", "gone")],
            &[(CONF, "my conf"), ("work/mine.txt", "mine"), ("app/images/addons/extra.cpio", "extra")],
        );
        let release = dir.join("release").join(format!("collabo-core-{PLATFORM}"));
        runtime(
            &release,
            &[(LAUNCHER, "new launcher"), (ENGINE, "new engine"), ("app/images/k", "kernel"), ("app/licenses/new.txt", "new"), (&long, "long")],
            &[(CONF, "default conf")],
        );
        change(&release);
        let archive = dir.join("release.tar.gz");
        let made = Command::new("tar")
            .args(["-czf", archive.to_str().unwrap(), "-C", dir.join("release").to_str().unwrap(), &format!("collabo-core-{PLATFORM}")])
            .status()
            .is_ok_and(|status| status.success());
        if !made {
            eprintln!("no tar command; skipped");
            return None;
        }
        Some((dir, base, archive))
    }

    fn as_it_was(base: &Path) {
        assert_eq!(read(base, LAUNCHER).as_deref(), Some("old launcher"));
        assert_eq!(read(base, ENGINE).as_deref(), Some("old engine"));
        assert_eq!(read(base, "app/licenses/gone.txt").as_deref(), Some("gone"));
        assert_eq!(read(base, "app/licenses/new.txt"), None);
        assert_eq!(read(base, CONF).as_deref(), Some("my conf"));
        assert!(!base.join("launcher.conf.new").exists());
        assert!(!base.join(WORK).exists());
    }

    #[test]
    fn an_update_replaces_the_runtime_and_keeps_what_is_yours() {
        let Some((dir, base, archive)) = scene("ok", &|_| {}) else { return };
        let seen = std::cell::RefCell::new(None);
        apply(&archive, &base, Some("9.9.9"), &|launcher| {
            *seen.borrow_mut() = std::fs::read_to_string(launcher).ok();
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.borrow().as_deref(), Some("new launcher"));
        assert_eq!(read(&base, ENGINE).as_deref(), Some("new engine"));
        assert_eq!(read(&base, "app/images/k").as_deref(), Some("kernel"));
        assert_eq!(read(&base, "app/licenses/new.txt").as_deref(), Some("new"));
        assert_eq!(read(&base, &format!("app/licenses/{}.txt", "x".repeat(120))).as_deref(), Some("long"));
        assert_eq!(read(&base, "app/licenses/gone.txt"), None);
        assert_eq!(read(&base, MANIFEST), read(&dir.join("release").join(format!("collabo-core-{PLATFORM}")), MANIFEST));
        assert_eq!(read(&base, CONF).as_deref(), Some("my conf"));
        assert!(!base.join("launcher.conf.new").exists());
        assert_eq!(read(&base, "work/mine.txt").as_deref(), Some("mine"));
        assert_eq!(read(&base, "app/images/addons/extra.cpio").as_deref(), Some("extra"));
        assert!(!base.join(WORK).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(base.join(LAUNCHER)).unwrap().permissions().mode() & 0o111, 0o111);
        }
        // A runtime without launcher.conf stays without one.
        std::fs::remove_file(base.join(CONF)).unwrap();
        apply(&archive, &base, Some("9.9.9"), &|_| Ok(())).unwrap();
        assert!(!base.join(CONF).exists() && !base.join("launcher.conf.new").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_new_launcher_that_does_not_run_puts_everything_back() {
        let Some((dir, base, archive)) = scene("undo", &|_| {}) else { return };
        let error = apply(&archive, &base, Some("9.9.9"), &|_| Err("exec format error".into())).unwrap_err();
        assert!(error.contains("exec format error") && error.contains("as it was"), "{error}");
        as_it_was(&base);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 0.2.17's archives held newer images than their manifests list: they go in all the same.
    #[test]
    fn files_that_are_not_what_the_manifest_sums_say_go_in_all_the_same() {
        let changed = |release: &Path| std::fs::write(join(release, ENGINE), "not what the manifest says").unwrap();
        let Some((dir, base, archive)) = scene("sums", &changed) else { return };
        apply(&archive, &base, Some("9.9.9"), &|_| Ok(())).unwrap();
        assert_eq!(read(&base, ENGINE).as_deref(), Some("not what the manifest says"));
        assert_eq!(read(&base, LAUNCHER).as_deref(), Some("new launcher"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_broken_archive_changes_nothing() {
        let missing = |release: &Path| std::fs::remove_file(join(release, "app/licenses/new.txt")).unwrap();
        let Some((dir, base, archive)) = scene("broken", &missing) else { return };
        let error = apply(&archive, &base, None, &|_| Ok(())).unwrap_err();
        assert!(error.contains("app/licenses/new.txt: its manifest.json lists it, and the archive does not have it"), "{error}");
        as_it_was(&base);
        let error = apply(&base.join(CONF), &base, None, &|_| Ok(())).unwrap_err();
        assert!(error.contains("not a .tar.gz or a .zip"), "{error}");
        as_it_was(&base);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A byte changed in the gzip trailer (its CRC): every file inflates, the check is at the end.
    #[test]
    fn a_tar_gz_whose_crc_does_not_match_changes_nothing() {
        let Some((dir, base, archive)) = scene("crc", &|_| {}) else { return };
        let mut bytes = std::fs::read(&archive).unwrap();
        let at = bytes.len() - 8;
        bytes[at] ^= 0xff;
        std::fs::write(&archive, bytes).unwrap();
        let error = apply(&archive, &base, None, &|_| Ok(())).unwrap_err();
        assert!(error.contains("a broken download?"), "{error}");
        as_it_was(&base);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `--update --apply` takes the newest archive an earlier `--update` downloaded.
    #[test]
    fn apply_finds_the_archive_downloaded_before() {
        let dir = std::env::temp_dir().join(format!("launcher-update-found-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (here, runtime) = (dir.join("here"), dir.join("runtime"));
        std::fs::create_dir_all(&here).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        let named = |version: &str| {
            download_name(&Release { tag: version.into(), name: archive_name(PLATFORM), size: 0, url: String::new() })
        };
        for name in [named("0.0.1"), named("999.0.0"), format!("{}.part", named("999.5.0")), named("not-a-version")] {
            std::fs::write(here.join(name), "").unwrap();
        }
        let other = if PLATFORM == "linux-x64" { "linux-arm64" } else { "linux-x64" };
        std::fs::write(here.join(format!("collabo-core-{other}-999.9.9.tar.gz")), "").unwrap();
        std::fs::write(runtime.join(named("v999.1.0")), "").unwrap();
        let dirs = [here.clone(), runtime.clone()];
        assert_eq!(downloaded(&dirs, false), Some((runtime.join(named("v999.1.0")), "v999.1.0".to_string())));
        std::fs::remove_file(runtime.join(named("v999.1.0"))).unwrap();
        assert_eq!(downloaded(&dirs, false), Some((here.join(named("999.0.0")), "999.0.0".to_string())));
        // Only older ones: none, unless --force.
        std::fs::remove_file(here.join(named("999.0.0"))).unwrap();
        assert_eq!(downloaded(&dirs, false), None);
        assert_eq!(downloaded(&dirs, true), Some((here.join(named("0.0.1")), "0.0.1".to_string())));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
