//! Shared pieces of the lfs package: `git lfs env`'s environment listing, fetch/prune
//! settings, time formatting.

use crate::config::cfg;
use crate::endpoint;
use std::sync::Mutex;

/// The git path variables as they were before canonicalizeEnvironment.
pub static OLD_ENV: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

pub fn git_line_ending() -> String {
    match cfg().git().get("core.autocrlf").unwrap_or_default().to_lowercase().as_str() {
        "true" | "t" | "1" => "\r\n".into(),
        _ => "\n".into(),
    }
}

/// strftime of a Unix time (UTC or local).
pub fn time_format(secs: i64, fmt: &str, utc: bool) -> String {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        if utc {
            libc::gmtime_r(&t, &mut tm);
        } else {
            libc::localtime_r(&t, &mut tm);
        }
    }
    let f = std::ffi::CString::new(fmt).unwrap();
    let mut buf = vec![0u8; 256];
    let n = unsafe { libc::strftime(buf.as_mut_ptr() as *mut libc::c_char, buf.len(), f.as_ptr(), &tm) };
    buf.truncate(n);
    String::from_utf8_lossy(&buf).into_owned()
}

pub struct FetchPruneConfig {
    pub fetch_recent_refs_days: i64,
    pub fetch_recent_refs_include_remotes: bool,
    pub fetch_recent_commits_days: i64,
    pub fetch_recent_always: bool,
    pub prune_offset_days: i64,
    pub prune_verify_remote_always: bool,
    pub prune_verify_unreachable_always: bool,
    pub prune_remote_name: String,
}

pub fn fetch_prune_config() -> FetchPruneConfig {
    let g = cfg().git();
    let remote = g.get("lfs.pruneremotetocheck").filter(|s| !s.is_empty()).unwrap_or_else(|| "origin".into());
    FetchPruneConfig {
        fetch_recent_refs_days: g.int("lfs.fetchrecentrefsdays", 7),
        fetch_recent_refs_include_remotes: g.bool("lfs.fetchrecentremoterefs", true),
        fetch_recent_commits_days: g.int("lfs.fetchrecentcommitsdays", 0),
        fetch_recent_always: g.bool("lfs.fetchrecentalways", false),
        prune_offset_days: g.int("lfs.pruneoffsetdays", 3),
        prune_verify_remote_always: g.bool("lfs.pruneverifyremotealways", false),
        prune_verify_unreachable_always: g.bool("lfs.pruneverifyunreachablealways", false),
        prune_remote_name: remote,
    }
}

pub fn concurrent_transfers() -> i64 {
    let v = cfg().git().int("lfs.concurrenttransfers", 8);
    if v < 1 {
        8
    } else {
        v
    }
}

/// The transfer adapters a direction offers (the manifest's adapter names), sorted.
pub fn adapter_names(upload: bool) -> Vec<String> {
    if cfg().basic_transfers_only() {
        return vec!["basic".into()];
    }
    let g = cfg().git();
    let mut names = vec!["basic".to_string(), "lfs-standalone-file".to_string(), "ssh".to_string()];
    if cfg().tus_transfers_allowed() && upload {
        names.push("tus".into());
    }
    let re = regex::Regex::new(r"^lfs\.(?i:customtransfer\.([^.]+))\.path$").unwrap();
    for k in g.vals.keys() {
        if let Some(m) = re.captures(k) {
            let sub = format!("customtransfer.{}", &m[1]);
            let dir = g.get(&format!("lfs.{sub}.direction")).unwrap_or_default().to_lowercase();
            let dir = if dir.is_empty() { "both".to_string() } else { dir };
            if (upload && (dir == "upload" || dir == "both")) || (!upload && (dir == "download" || dir == "both")) {
                if !names.contains(&m[1].to_string()) {
                    names.push(m[1].to_string());
                }
            }
        }
    }
    names.sort();
    names
}

/// lfs.Environ: the lines `git lfs env` prints after the endpoints.
pub fn environ() -> Vec<String> {
    let c = cfg();
    let download = endpoint::access_for(&endpoint::endpoint("download", &c.remote()).url).0;
    let upload = endpoint::access_for(&endpoint::endpoint("upload", &c.push_remote()).url).0;
    let fp = fetch_prune_config();
    let fs = c.filesystem();
    let mut env = vec![
        format!("LocalWorkingDir={}", c.local_working_dir()),
        format!("LocalGitDir={}", c.local_git_dir()),
        format!("LocalGitStorageDir={}", c.local_git_storage_dir()),
        format!("LocalMediaDir={}", c.lfs_object_dir()),
        format!("LocalReferenceDirs={}", fs.reference_dirs.join(", ")),
        format!("TempDir={}", c.temp_dir()),
        format!("ConcurrentTransfers={}", concurrent_transfers()),
        format!("TusTransfers={}", c.tus_transfers_allowed()),
        format!("BasicTransfersOnly={}", c.basic_transfers_only()),
        format!("SkipDownloadErrors={}", c.skip_download_errors()),
        format!("FetchRecentAlways={}", fp.fetch_recent_always),
        format!("FetchRecentRefsDays={}", fp.fetch_recent_refs_days),
        format!("FetchRecentCommitsDays={}", fp.fetch_recent_commits_days),
        format!("FetchRecentRefsIncludeRemotes={}", fp.fetch_recent_refs_include_remotes),
        format!("PruneOffsetDays={}", fp.prune_offset_days),
        format!("PruneVerifyRemoteAlways={}", fp.prune_verify_remote_always),
        format!("PruneVerifyUnreachableAlways={}", fp.prune_verify_unreachable_always),
        format!("PruneRemoteName={}", fp.prune_remote_name),
        format!("LfsStorageDir={}", c.lfs_storage_dir()),
        format!("AccessDownload={}", download.mode()),
        format!("AccessUpload={}", upload.mode()),
        format!("DownloadTransfers={}", adapter_names(false).join(",")),
        format!("UploadTransfers={}", adapter_names(true).join(",")),
    ];
    let ex = c.fetch_exclude_paths();
    if !ex.is_empty() {
        env.push(format!("FetchExclude={}", ex.join(", ")));
    }
    let inc = c.fetch_include_paths();
    if !inc.is_empty() {
        env.push(format!("FetchInclude={}", inc.join(", ")));
    }
    for e in c.extensions().values() {
        env.push(format!("Extension[{}]={}", e.priority, e.name));
    }
    let old = OLD_ENV.lock().unwrap().clone();
    for (k, v) in std::env::vars_os() {
        let k = k.to_string_lossy().into_owned();
        if !k.starts_with("GIT_") {
            continue;
        }
        match old.iter().find(|(o, _)| *o == k) {
            Some((_, ov)) => env.push(format!("{k}={ov}")),
            None => env.push(format!("{k}={}", v.to_string_lossy())),
        }
    }
    env
}

/// GIT_LOG_STATS: HTTP statistics into .git/lfs/logs/http (set up before a command runs).
pub fn setup_http_logger() {
    if std::env::var("GIT_LOG_STATS").map_or(true, |v| v.is_empty()) {
        return;
    }
    let base = format!("{}/http", cfg().local_log_dir());
    if let Err(e) = crate::tools::mkdir_all(&base, cfg().repository_permissions(false)) {
        eprintln!("Error logging HTTP stats: {}", crate::tools::io_err(&e));
        return;
    }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let path = format!("{base}/http-{secs}.log");
    match std::fs::File::create(&path) {
        Ok(f) => crate::http::set_stats_log(f),
        Err(e) => eprintln!("Error logging HTTP stats: {}", crate::tools::path_err("open", &path, &e)),
    }
}
