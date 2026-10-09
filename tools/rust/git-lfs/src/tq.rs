//! The transfer queue (tq): objects added are sent to the batch API in batches; the transfer
//! adapter the server picks (basic, tus, ssh, a custom one or the standalone file adapter)
//! moves them with concurrent workers; failures are tried again with backoff (Retry-After
//! honoured), and a meter reports progress.

use crate::config::cfg;
use crate::endpoint;
use crate::errors::{Error, Kind, Result};
use crate::gitcmd::Ref;
use crate::http::{self, Hooks, Request};
use crate::lfsapi;
use crate::pointer::Pointer;
use crate::ssh::SshTransfer;
use crate::tasklog::{Logger, Update};
use crate::tools;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Upload,
    Download,
    Checkout,
}

impl Direction {
    pub fn progress(&self) -> &'static str {
        match self {
            Direction::Checkout => "Checking out LFS objects",
            Direction::Download => "Downloading LFS objects",
            Direction::Upload => "Uploading LFS objects",
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Direction::Checkout => "checkout",
            Direction::Download => "download",
            Direction::Upload => "upload",
        }
    }
}

// The batch API's objects.

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Action {
    #[serde(default, deserialize_with = "crate::http::null_default")]
    pub href: String,
    #[serde(default, deserialize_with = "crate::http::null_default", skip_serializing_if = "BTreeMap::is_empty")]
    pub header: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "crate::http::null_default", skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, deserialize_with = "crate::http::null_default", skip_serializing_if = "is_zero")]
    pub expires_in: i64,
    #[serde(skip)]
    pub id: String,
    #[serde(skip)]
    pub token: String,
    #[serde(skip)]
    pub created_at: Option<SystemTime>,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

impl Action {
    fn expires_at_time(&self) -> Option<SystemTime> {
        self.expires_at.as_deref().and_then(tools::parse_rfc3339).filter(|t| tools::unix_secs(*t) > -62135596800)
    }
    /// IsExpiredWithin: (expiration, whether it falls within `d` from now).
    pub fn expired_within(&self, d: Duration) -> (Option<SystemTime>, bool) {
        let exp = if self.expires_in != 0 {
            Some(self.created_at.unwrap_or_else(SystemTime::now) + Duration::from_secs(self.expires_in.max(0) as u64))
        } else {
            self.expires_at_time()
        };
        match exp {
            None => (None, false),
            Some(t) => (Some(t), t < SystemTime::now() + d),
        }
    }
    /// The action as Go's json encodes it (custom transfer messages).
    pub fn to_go_json(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("href".into(), self.href.clone().into());
        if !self.header.is_empty() {
            m.insert("header".into(), serde_json::to_value(&self.header).unwrap());
        }
        m.insert("expires_at".into(), tools::format_rfc3339_utc(self.expires_at_time()).into());
        if self.expires_in != 0 {
            m.insert("expires_in".into(), self.expires_in.into());
        }
        serde_json::Value::Object(m)
    }
}

pub type ActionSet = BTreeMap<String, Action>;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ObjectError {
    #[serde(default, deserialize_with = "crate::http::null_default")]
    pub code: i64,
    #[serde(default, deserialize_with = "crate::http::null_default")]
    pub message: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Transfer {
    #[serde(default, deserialize_with = "crate::http::null_default", skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, deserialize_with = "crate::http::null_default", skip_serializing_if = "String::is_empty")]
    pub oid: String,
    #[serde(default, deserialize_with = "crate::http::null_default")]
    pub size: i64,
    #[serde(default, deserialize_with = "crate::http::null_default", skip_serializing_if = "std::ops::Not::not")]
    pub authenticated: bool,
    #[serde(default, deserialize_with = "crate::http::null_default", skip_serializing_if = "BTreeMap::is_empty")]
    pub actions: ActionSet,
    #[serde(rename = "_links", default, deserialize_with = "crate::http::null_default", skip_serializing_if = "Option::is_none")]
    pub links: Option<ActionSet>,
    #[serde(default, deserialize_with = "crate::http::null_default", skip_serializing_if = "Option::is_none")]
    pub error: Option<ObjectError>,
    #[serde(default, deserialize_with = "crate::http::null_default", skip_serializing_if = "String::is_empty")]
    pub path: String,
}

fn action_get(set: &ActionSet, rel: &str) -> Result<Option<Action>> {
    let Some(a) = set.get(rel) else { return Ok(None) };
    if let (Some(at), true) = a.expired_within(Duration::from_secs(5)) {
        return Err(Error::new(format!("action {} expires at {}", tools::quote(rel), tools::format_rfc822_local(at))).retriable());
    }
    Ok(Some(a.clone()))
}

impl Transfer {
    /// Rel: the action (or link) for an operation.
    pub fn rel(&self, name: &str) -> Result<Option<Action>> {
        if let Some(a) = action_get(&self.actions, name)? {
            return Ok(Some(a));
        }
        if let Some(l) = &self.links {
            return action_get(l, name);
        }
        Ok(None)
    }
}

// Errors of objects.

pub fn object_missing_error(name: &str, oid: &str) -> Error {
    let mut e = Error::new(format!("missing object: {name} ({oid})"));
    e.kind = Kind::NotFound;
    e
}

pub fn corrupt_object_error(name: &str, oid: &str) -> Error {
    Error::new(format!("corrupt object: {name} ({oid})"))
}

// The progress meter.

struct MeterState {
    finished_files: i64,
    transferring_files: i64,
    estimated_bytes: i64,
    last_bytes: i64,
    current_bytes: i64,
    sample_count: u64,
    avg_bytes: f64,
    last_avg: Instant,
    estimated_files: i64,
    file_index: HashMap<String, i64>,
    logger: Option<std::fs::File>,
}

pub struct Meter {
    st: Mutex<MeterState>,
    paused: AtomicBool,
    pub dry_run: bool,
    direction: Mutex<Direction>,
    tx: Mutex<Option<Sender<Update>>>,
    rx: Mutex<Option<Receiver<Update>>>,
}

impl Meter {
    pub fn new(direction: Direction, dry_run: bool) -> Arc<Meter> {
        let (tx, rx) = channel();
        Arc::new(Meter {
            st: Mutex::new(MeterState {
                finished_files: 0,
                transferring_files: 0,
                estimated_bytes: 0,
                last_bytes: 0,
                current_bytes: 0,
                sample_count: 0,
                avg_bytes: 0.0,
                last_avg: Instant::now(),
                estimated_files: 0,
                file_index: HashMap::new(),
                logger: None,
            }),
            paused: AtomicBool::new(false),
            dry_run,
            direction: Mutex::new(direction),
            tx: Mutex::new(Some(tx)),
            rx: Mutex::new(Some(rx)),
        })
    }

    /// buildProgressMeter: with the GIT_LFS_PROGRESS log.
    pub fn build(dry_run: bool, d: Direction) -> Arc<Meter> {
        let m = Meter::new(d, dry_run);
        m.logger_from_env();
        m
    }

    pub fn logger_from_env(&self) {
        let name = cfg().os.get("GIT_LFS_PROGRESS").unwrap_or_default();
        if name.is_empty() {
            return;
        }
        let err = |e: &str| eprintln!("Error creating progress logger: {e}");
        if !name.starts_with('/') {
            err("GIT_LFS_PROGRESS must be an absolute path");
            return;
        }
        if let Some(d) = std::path::Path::new(&name).parent() {
            if let Err(e) = tools::mkdir_all(d, cfg().repository_permissions(false)) {
                err(&tools::io_err(&e));
                return;
            }
        }
        match std::fs::OpenOptions::new().append(true).create(true).open(&name) {
            Ok(f) => self.st.lock().unwrap().logger = Some(f),
            Err(e) => err(&tools::path_err("open", &name, &e).to_string()),
        }
    }

    /// Hands the meter's updates to a logger (logger.Enqueue(meter)).
    pub fn enqueue(&self, logger: &Logger) {
        if let Some(rx) = self.rx.lock().unwrap().take() {
            logger.enqueue_channel(rx, true);
        }
    }

    pub fn set_direction(&self, d: Direction) {
        *self.direction.lock().unwrap() = d;
    }
    pub fn start(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }
    pub fn add(&self, size: i64) {
        {
            let mut s = self.st.lock().unwrap();
            s.estimated_files += 1;
            s.estimated_bytes += size;
        }
        self.update(false);
    }
    pub fn skip(&self, size: i64) {
        {
            let mut s = self.st.lock().unwrap();
            s.finished_files += 1;
            s.current_bytes += size;
        }
        self.update(false);
    }
    pub fn start_transfer(&self, name: &str) {
        {
            let mut s = self.st.lock().unwrap();
            s.transferring_files += 1;
            let idx = s.transferring_files;
            s.file_index.insert(name.to_string(), idx);
        }
        self.update(false);
    }
    pub fn transfer_bytes(&self, direction: &str, name: &str, read: i64, total: i64, current: i64) {
        {
            let mut s = self.st.lock().unwrap();
            let now = Instant::now();
            let since = now.duration_since(s.last_avg);
            s.current_bytes += current;
            s.last_bytes += current;
            if since > Duration::from_secs(1) {
                s.last_avg = now;
                let bps = s.last_bytes as f64 / since.as_secs_f64();
                s.avg_bytes = (s.avg_bytes * s.sample_count as f64 + bps) / (s.sample_count as f64 + 1.0);
                s.last_bytes = 0;
                s.sample_count += 1;
            }
            let idx = s.file_index.get(name).copied().unwrap_or(0);
            let est = s.estimated_files;
            if let Some(l) = s.logger.as_mut() {
                let line = format!("{} {}/{} {} {}\n", direction, idx, est, tools::format_fraction(read, total), name);
                if l.write_all(line.as_bytes()).is_err() {
                    s.logger = None;
                }
            }
        }
        self.update(false);
    }
    pub fn finish_transfer(&self, name: &str) {
        {
            let mut s = self.st.lock().unwrap();
            s.finished_files += 1;
            s.file_index.remove(name);
        }
        self.update(false);
    }
    pub fn flush(&self) {
        self.update(true);
    }
    /// Finish: the last update, then the task ends.
    pub fn finish(&self) {
        self.update(false);
        self.tx.lock().unwrap().take();
    }
    fn update(&self, force: bool) {
        let s = self.st.lock().unwrap();
        if self.dry_run || s.estimated_files == 0 || self.paused.load(Ordering::SeqCst) {
            return;
        }
        let pct = 100.0 * s.finished_files as f64 / s.estimated_files as f64;
        let line = format!(
            "{}: {:3.0}% ({}/{}), {} | {}",
            self.direction.lock().unwrap().progress(),
            pct,
            s.finished_files,
            s.estimated_files,
            tools::format_bytes(s.current_bytes.max(0) as u64),
            tools::format_byte_rate(s.avg_bytes.max(0.0) as u64, 1.0)
        );
        drop(s);
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(Update { s: line, at: Instant::now(), force });
        }
    }
}

// The manifest: which adapters there are, and the retry settings.

#[derive(Clone)]
struct CustomAdapter {
    path: String,
    args: String,
    concurrent: bool,
}

pub struct Concrete {
    pub max_retries: i64,
    pub max_retry_delay: i64,
    pub max_retry_time: i64,
    pub concurrent_transfers: i64,
    basic_only: bool,
    pub standalone_agent: String,
    tus_allowed: bool,
    custom_download: BTreeMap<String, CustomAdapter>,
    custom_upload: BTreeMap<String, CustomAdapter>,
    pub ssh: Option<Arc<SshTransfer>>,
}

pub struct Manifest {
    operation: String,
    remote: String,
    m: OnceLock<Concrete>,
}

const STANDALONE_FILE: &str = "lfs-standalone-file";

impl Manifest {
    pub fn new(operation: &str, remote: &str) -> Arc<Manifest> {
        Arc::new(Manifest { operation: operation.to_string(), remote: remote.to_string(), m: OnceLock::new() })
    }

    /// getTransferManifestOperationRemote: one per (operation, remote).
    pub fn get(operation: &str, remote: &str) -> Arc<Manifest> {
        static ALL: Mutex<Vec<(String, String, Arc<Manifest>)>> = Mutex::new(Vec::new());
        let mut all = ALL.lock().unwrap();
        if let Some(m) = all.iter().find(|(o, r, _)| o == operation && r == remote) {
            return m.2.clone();
        }
        let m = Manifest::new(operation, remote);
        all.push((operation.into(), remote.into(), m.clone()));
        m
    }

    pub fn upgrade(&self) -> &Concrete {
        self.m.get_or_init(|| Concrete::new(&self.operation, &self.remote))
    }

}

impl Concrete {
    fn new(operation: &str, remote: &str) -> Concrete {
        let g = cfg().git();
        let ssh = crate::ssh::transfer_for(operation, remote);
        let mut m = Concrete {
            max_retries: 0,
            max_retry_delay: 0,
            max_retry_time: 0,
            concurrent_transfers: 0,
            basic_only: g.bool("lfs.basictransfersonly", false),
            standalone_agent: String::new(),
            tus_allowed: g.bool("lfs.tustransfers", false),
            custom_download: BTreeMap::new(),
            custom_upload: BTreeMap::new(),
            ssh,
        };
        if let v @ 1.. = g.int("lfs.transfer.maxretries", 0) {
            m.max_retries = v;
        }
        let d = g.int("lfs.transfer.maxretrydelay", -1);
        if d > -1 {
            m.max_retry_delay = d;
        }
        if let v @ 1.. = g.int("lfs.transfer.maxretrytime", 0) {
            m.max_retry_time = v;
        }
        if let v @ 1.. = g.int("lfs.concurrenttransfers", 0) {
            m.concurrent_transfers = v;
        }
        m.standalone_agent = find_standalone_transfer(operation, remote);
        // Custom adapters (and the standalone file one).
        let sf = CustomAdapter { path: "git-lfs".into(), args: "standalone-file".into(), concurrent: false };
        m.custom_download.insert(STANDALONE_FILE.into(), sf.clone());
        m.custom_upload.insert(STANDALONE_FILE.into(), sf);
        let re = regex::Regex::new(r"lfs\.((?i)customtransfer\.([^.]+))\.path").unwrap();
        for k in g.vals.keys() {
            let Some(c) = re.captures(k) else { continue };
            let sub = &c[1];
            let name = c[2].to_string();
            let a = CustomAdapter {
                path: g.get(k).unwrap_or_default(),
                args: g.get(&format!("lfs.{sub}.args")).unwrap_or_default(),
                concurrent: g.bool(&format!("lfs.{sub}.concurrent"), true),
            };
            let dir = g.get(&format!("lfs.{sub}.direction")).unwrap_or_default().to_lowercase();
            let dir = if dir.is_empty() { "both".to_string() } else { dir };
            let standard = |n: &str, up: bool| n == "basic" || n == "ssh" || (up && n == "tus" && m.tus_allowed);
            if dir == "download" || dir == "both" {
                if standard(&name, false) {
                    eprintln!("warning: custom download transfer adapter {} ignored due to conflict with standard adapter", tools::quote(&name));
                } else {
                    m.custom_download.insert(name.clone(), a.clone());
                }
            }
            if dir == "upload" || dir == "both" {
                if standard(&name, true) {
                    eprintln!("warning: custom upload transfer adapter {} ignored due to conflict with standard adapter", tools::quote(&name));
                } else {
                    m.custom_upload.insert(name.clone(), a);
                }
            }
        }
        if m.max_retries < 1 {
            m.max_retries = 8;
        }
        if m.max_retry_delay < 1 {
            m.max_retry_delay = 10;
        }
        if m.max_retry_time < 1 {
            m.max_retry_time = 300;
        }
        if m.concurrent_transfers < 1 {
            m.concurrent_transfers = 8;
        }
        if let Some(s) = &m.ssh {
            if !s.is_multiplexing_enabled() {
                m.concurrent_transfers = 1;
            }
        }
        if !m.standalone_agent.is_empty() {
            let custom = match operation {
                "upload" => m.custom_upload.contains_key(&m.standalone_agent),
                "download" => m.custom_download.contains_key(&m.standalone_agent),
                _ => false,
            };
            if !custom {
                crate::trace!("standalone agent {} is not a registered custom transfer adapter; ignoring", tools::quote(&m.standalone_agent));
                m.standalone_agent.clear();
            }
        }
        m
    }

    pub fn adapter_names(&self, dir: Direction) -> Vec<String> {
        if self.basic_only {
            return vec!["basic".into()];
        }
        let mut n = vec!["basic".to_string(), "ssh".to_string()];
        let custom = if dir == Direction::Upload { &self.custom_upload } else { &self.custom_download };
        if dir == Direction::Upload && self.tus_allowed {
            n.push("tus".into());
        }
        n.extend(custom.keys().cloned());
        n
    }

    fn new_adapter(&self, name: &str, dir: Direction) -> Option<AdapterKind> {
        let custom = if dir == Direction::Upload { &self.custom_upload } else { &self.custom_download };
        match name {
            "basic" => Some(if dir == Direction::Upload { AdapterKind::BasicUpload } else { AdapterKind::BasicDownload }),
            "tus" if dir == Direction::Upload && self.tus_allowed => Some(AdapterKind::Tus),
            "ssh" => Some(AdapterKind::Ssh(self.ssh.clone())),
            n => custom.get(n).map(|c| AdapterKind::Custom { path: c.path.clone(), args: c.args.clone(), concurrent: c.concurrent, standalone: !self.standalone_agent.is_empty() }),
        }
    }

    fn new_adapter_or_default(&self, name: &str, dir: Direction) -> (String, AdapterKind) {
        let name = if name.is_empty() { "basic" } else { name };
        match self.new_adapter(name, dir) {
            Some(a) => (name.to_string(), a),
            None => {
                crate::trace!("Defaulting to basic transfer adapter since {} did not exist", tools::quote(name));
                ("basic".into(), self.new_adapter("basic", dir).unwrap())
            }
        }
    }
}

fn find_standalone_transfer(operation: &str, remote: &str) -> String {
    if operation.is_empty() || remote.is_empty() {
        return cfg().git().get("lfs.standalonetransferagent").unwrap_or_default();
    }
    let ep = endpoint::endpoint(operation, remote);
    match crate::config::url_get("lfs", &ep.url, "standalonetransferagent") {
        Some(v) => v,
        None if ep.url.starts_with("file://") => STANDALONE_FILE.into(),
        None => String::new(),
    }
}

// The batch API.

pub struct BatchResponse {
    pub objects: Vec<Transfer>,
    pub transfer: String,
}

#[derive(Deserialize)]
struct BatchResponseJson {
    #[serde(default, deserialize_with = "crate::http::null_default")]
    objects: Vec<Transfer>,
    #[serde(default, deserialize_with = "crate::http::null_default")]
    transfer: String,
    #[serde(default, deserialize_with = "crate::http::null_default")]
    hash_algo: String,
}

pub fn batch(m: &Concrete, dir: Direction, remote: &str, remote_ref: Option<&Ref>, objects: &[Transfer]) -> Result<BatchResponse> {
    let empty = BatchResponse { objects: vec![], transfer: String::new() };
    if objects.is_empty() {
        return Ok(empty);
    }
    let names = m.adapter_names(dir);
    let refname = remote_ref.map(|r| r.refspec()).unwrap_or_default();
    if let Some(ssh) = &m.ssh {
        return ssh_batch(ssh, objects, &refname);
    }
    let mut req_obj = serde_json::Map::new();
    req_obj.insert("operation".into(), dir.as_str().into());
    let objs: Vec<serde_json::Value> = objects.iter().map(|t| serde_json::json!({"oid": t.oid, "size": t.size})).collect();
    req_obj.insert("objects".into(), objs.into());
    if !(names.len() == 1 && names[0] == "basic") {
        req_obj.insert("transfers".into(), serde_json::to_value(&names).unwrap());
    }
    let mut r = serde_json::Map::new();
    if !refname.is_empty() {
        r.insert("name".into(), refname.into());
    }
    req_obj.insert("ref".into(), serde_json::Value::Object(r));
    req_obj.insert("hash_algo".into(), "sha256".into());
    let ep = endpoint::endpoint(dir.as_str(), remote);
    let requested_at = SystemTime::now();
    let mut req = http::client().new_request("POST", &ep, "objects/batch", Some(&serde_json::Value::Object(req_obj))).map_err(|e| e.wrap("batch request"))?;
    crate::trace!("api: batch {} files", objects.len());
    req.stats_key = Some("lfs.batch".into());
    req.retries = Some(m.max_retries);
    let (res, err) = lfsapi::do_api_request_with_auth(remote, &mut req, &mut Hooks::default());
    if let Some(e) = err {
        crate::trace!("api error: {}", e);
        return Err(e.wrap("batch response"));
    }
    let res = res.unwrap();
    let b: BatchResponseJson = http::decode_json(&res).map_err(|e| e.into_error().wrap("batch response"))?;
    if !b.hash_algo.is_empty() && b.hash_algo != "sha256" {
        return Err(Error::new("unsupported hash algorithm").wrap("batch response"));
    }
    if res.status != 200 {
        return Err(http::status_code_error(&res));
    }
    let mut objects = b.objects;
    for o in &mut objects {
        for a in o.actions.values_mut() {
            a.created_at = Some(requested_at);
        }
    }
    Ok(BatchResponse { objects, transfer: b.transfer })
}

fn ssh_batch(ssh: &SshTransfer, objects: &[Transfer], refname: &str) -> Result<BatchResponse> {
    let lines: Vec<String> = objects.iter().map(|o| format!("{} {}", o.oid, o.size)).collect();
    crate::trace!("api: batch {} files", objects.len());
    let requested_at = SystemTime::now();
    let mut args = vec!["transfer=ssh".to_string(), "hash-algo=sha256".to_string()];
    args.push(format!("refname={refname}"));
    let conn = ssh.connection(0).map_err(|e| e.wrap("could not get connection for batch request"))?;
    let (status, args, mut lines) = {
        let mut c = conn.lock().unwrap();
        c.send_message_with_lines("batch", &args, &lines).map_err(|e| e.wrap("batch request"))?;
        c.read_status_with_lines().map_err(|e| e.wrap("batch response"))?
    };
    if status != 200 {
        let msg = lines.first().cloned().unwrap_or_else(|| "no message provided".into());
        return Err(Error::new(format!("batch response: status {status} from server ({msg})")));
    }
    for a in &args {
        if let Some(v) = a.strip_prefix("hash-algo=") {
            if v != "sha256" {
                return Err(Error::new(format!("batch response: unsupported hash algorithm: {}", tools::quote(v))));
            }
        }
    }
    lines.sort();
    let mut out: Vec<Transfer> = vec![];
    for line in &lines {
        let e: Vec<&str> = line.split(' ').collect();
        if e.len() < 3 {
            return Err(Error::new(format!("batch response: malformed response: {}", tools::quote(line))));
        }
        if out.last().is_none_or(|t| t.oid != e[0]) {
            out.push(Transfer::default());
        }
        let t = out.last_mut().unwrap();
        t.oid = e[0].to_string();
        t.size = e[1].parse().map_err(|_| Error::new(format!("batch response: invalid size: {}", e[1])))?;
        if e[2] == "noop" {
            continue;
        }
        let mut a = Action { created_at: Some(requested_at), ..Default::default() };
        for x in &e[3..] {
            if let Some(v) = x.strip_prefix("id=") {
                a.id = v.into();
            } else if let Some(v) = x.strip_prefix("token=") {
                a.token = v.into();
            } else if let Some(v) = x.strip_prefix("expires-in=") {
                a.expires_in = v.parse().map_err(|_| Error::new(format!("batch response: invalid expires-in: {x}")))?;
            } else if let Some(v) = x.strip_prefix("expires-at=") {
                if tools::parse_rfc3339(v).is_none() {
                    return Err(Error::new(format!("batch response: invalid expires-at: {x}")));
                }
                a.expires_at = Some(v.into());
            }
        }
        t.actions.insert(e[2].to_string(), a);
    }
    Ok(BatchResponse { objects: out, transfer: "ssh".into() })
}

// Transfer adapters.

#[derive(Clone)]
enum AdapterKind {
    BasicDownload,
    BasicUpload,
    Tus,
    Ssh(Option<Arc<SshTransfer>>),
    Custom { path: String, args: String, concurrent: bool, standalone: bool },
}

/// name, total size, read so far, read since the last call.
pub type ProgressCb = Arc<dyn Fn(&str, i64, i64, i64) + Send + Sync>;

struct Job {
    t: Transfer,
    results: Sender<(Transfer, Option<Error>)>,
}

struct AuthGate {
    done: Mutex<bool>,
    cv: Condvar,
}

impl AuthGate {
    fn open(&self) {
        *self.done.lock().unwrap() = true;
        self.cv.notify_all();
    }
    fn wait(&self) {
        let mut d = self.done.lock().unwrap();
        while !*d {
            d = self.cv.wait(d).unwrap();
        }
    }
}

struct WorkerEnv {
    name: String,
    kind: AdapterKind,
    dir: Direction,
    remote: String,
    debugging: bool,
    cb: ProgressCb,
    gate: Arc<AuthGate>,
    original_concurrency: i64,
}

impl WorkerEnv {
    fn trace(&self, msg: impl FnOnce() -> String) {
        if self.debugging {
            crate::trace!("{}", msg());
        }
    }
}

struct Adapter {
    name: String,
    jobs: Option<Sender<Job>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    debugging: bool,
}

/// The per-worker state of an adapter: a custom adapter's process.
enum WorkerCtx {
    None,
    /// The basic download adapter's: whether its zstd decoder was set up.
    Download(bool),
    Custom(CustomProcess),
    Ssh(usize),
}

impl Adapter {
    fn begin(name: &str, kind: AdapterKind, dir: Direction, concurrency: i64, remote: &str, cb: ProgressCb) -> Result<Adapter> {
        let os = &cfg().os;
        let debugging = os.bool("GIT_TRANSFER_TRACE", false) || (!matches!(kind, AdapterKind::Ssh(_)) && os.bool("GIT_CURL_VERBOSE", false));
        let workers_n = match &kind {
            AdapterKind::Custom { concurrent: false, .. } => 1,
            _ => concurrency.max(1),
        };
        if debugging {
            crate::trace!("xfer: adapter {} Begin() with {} workers", tools::quote(name), workers_n);
        }
        let (tx, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let gate = Arc::new(AuthGate { done: Mutex::new(false), cv: Condvar::new() });
        let env = Arc::new(WorkerEnv {
            name: name.to_string(),
            kind: kind.clone(),
            dir,
            remote: remote.to_string(),
            debugging,
            cb,
            gate,
            original_concurrency: concurrency,
        });
        let mut workers = vec![];
        for i in 0..workers_n as usize {
            let ctx = worker_starting(&env, i)?;
            let env = env.clone();
            let rx = rx.clone();
            let h = std::thread::Builder::new().spawn(move || worker(env, i, ctx, rx)).map_err(|e| Error::new(e.to_string()))?;
            workers.push(h);
        }
        if debugging {
            crate::trace!("xfer: adapter {} started", tools::quote(name));
        }
        Ok(Adapter { name: name.to_string(), jobs: Some(tx), workers, debugging })
    }

    /// Runs the transfers; their results as they finish.
    fn add(&self, transfers: Vec<Transfer>) -> Receiver<(Transfer, Option<Error>)> {
        let (rtx, rrx) = channel();
        for t in transfers {
            if let Some(j) = &self.jobs {
                let _ = j.send(Job { t, results: rtx.clone() });
            }
        }
        rrx
    }

    fn end(mut self) {
        if self.debugging {
            crate::trace!("xfer: adapter {} End()", tools::quote(&self.name));
        }
        self.jobs.take();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        if self.debugging {
            crate::trace!("xfer: adapter {} stopped", tools::quote(&self.name));
        }
    }
}

fn worker(env: Arc<WorkerEnv>, n: usize, mut ctx: WorkerCtx, rx: Arc<Mutex<Receiver<Job>>>) {
    let name = tools::quote(&env.name);
    env.trace(|| format!("xfer: adapter {name} worker {n} starting"));
    let mut signal_auth = n == 0;
    if n > 0 {
        env.trace(|| format!("xfer: adapter {name} worker {n} waiting for Auth"));
        env.gate.wait();
        env.trace(|| format!("xfer: adapter {name} worker {n} auth signal received"));
    }
    loop {
        let job = rx.lock().unwrap().recv();
        let Ok(job) = job else { break };
        let t = job.t;
        env.trace(|| format!("xfer: adapter {name} worker {n} processing job for {}", tools::quote(&t.oid)));
        let gate = env.gate.clone();
        let mut auth_ok = || {
            if signal_auth {
                gate.open();
                signal_auth = false;
            }
        };
        let r = if t.size < 0 {
            Err(Error::new(format!("object {} has invalid size (got: {})", tools::quote(&t.oid), t.size)))
        } else {
            do_transfer(&env, &mut ctx, &t, &mut auth_ok)
        };
        let oid = t.oid.clone();
        let _ = job.results.send((t, r.err()));
        env.trace(|| format!("xfer: adapter {name} worker {n} finished job for {}", tools::quote(&oid)));
    }
    if signal_auth {
        env.gate.open();
    }
    env.trace(|| format!("xfer: adapter {name} worker {n} stopping"));
    match ctx {
        WorkerCtx::Custom(p) => worker_ending_custom(&env, p),
        WorkerCtx::Download(true) => crate::trace!("http: closed zstd decoder"),
        _ => {}
    }
}

fn worker_starting(env: &WorkerEnv, n: usize) -> Result<WorkerCtx> {
    match &env.kind {
        AdapterKind::Custom { path, args, concurrent, .. } => {
            env.trace(|| format!("xfer: starting up custom transfer process {} for worker {}", tools::quote(&env.name), n));
            let p = CustomProcess::start(env, n, path, args, *concurrent)?;
            Ok(WorkerCtx::Custom(p))
        }
        AdapterKind::Ssh(t) => {
            let Some(t) = t else { return Err(Error::new("missing SSH transfer connection for SSH transfer adapter")) };
            t.set_connection_count_at_least(n + 1);
            Ok(WorkerCtx::Ssh(n))
        }
        AdapterKind::BasicDownload => Ok(WorkerCtx::Download(false)),
        _ => Ok(WorkerCtx::None),
    }
}

fn do_transfer(env: &WorkerEnv, ctx: &mut WorkerCtx, t: &Transfer, auth_ok: &mut dyn FnMut()) -> Result<()> {
    match (&env.kind, ctx) {
        (AdapterKind::BasicDownload, WorkerCtx::Download(z)) => basic_download(env, t, auth_ok, z),
        (AdapterKind::BasicDownload, _) => basic_download(env, t, auth_ok, &mut false),
        (AdapterKind::BasicUpload, _) => basic_upload(env, t, auth_ok),
        (AdapterKind::Tus, _) => tus_upload(env, t, auth_ok),
        (AdapterKind::Custom { standalone, .. }, WorkerCtx::Custom(p)) => custom_transfer(env, p, t, *standalone, auth_ok),
        (AdapterKind::Custom { .. }, _) => Err(Error::new(format!("custom transfer {} was not properly initialized, see previous errors", tools::quote(&env.name)))),
        (AdapterKind::Ssh(Some(s)), WorkerCtx::Ssh(n)) => {
            auth_ok();
            if env.dir == Direction::Upload {
                ssh_upload(env, s, *n, t)
            } else {
                ssh_download(env, s, *n, t)
            }
        }
        _ => Err(Error::new("missing SSH transfer connection for SSH transfer adapter")),
    }
}

fn progress(env: &WorkerEnv, t: &Transfer, read: i64, current: i64) {
    (env.cb)(&t.name, t.size, read, current);
}

/// advanceCallbackProgress.
fn advance_progress(env: &WorkerEnv, t: &Transfer, n: i64) {
    if n > 0 {
        progress(env, t, n, n);
    }
}

fn incomplete_dir() -> String {
    let d = format!("{}/incomplete", cfg().lfs_storage_dir());
    if tools::mkdir_all(&d, cfg().repository_permissions(true)).is_err() {
        return std::env::temp_dir().display().to_string();
    }
    d
}

/// tools.TempFile: a new file named after `pattern` in `dir`.
fn temp_file_in(dir: &str, pattern: &str) -> Result<(std::fs::File, String)> {
    use std::os::unix::fs::PermissionsExt;
    for i in 0..10000u32 {
        let r = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos() ^ (std::process::id() << 12) ^ i.wrapping_mul(2654435761);
        let name = format!("{dir}/{pattern}{r}");
        match std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&name) {
            Ok(f) => {
                let _ = std::fs::set_permissions(&name, std::fs::Permissions::from_mode(cfg().repository_permissions(false)));
                return Ok((f, name));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(tools::path_err("open", &name, &e)),
        }
    }
    Err(Error::new("could not create a temporary file"))
}

/// tools.RenameFileCopyPermissions: the file into place with the permissions of the one it
/// replaces (or the repository's).
pub fn rename_copy_permissions(src: &str, dst: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = match std::fs::metadata(dst) {
        Ok(m) => m.permissions().mode() & 0o777,
        Err(_) => cfg().repository_permissions(false),
    };
    let _ = std::fs::set_permissions(src, std::fs::Permissions::from_mode(mode));
    if let Some(p) = std::path::Path::new(dst).parent() {
        let _ = tools::mkdir_all(p, cfg().repository_permissions(true));
    }
    std::fs::rename(src, dst).map_err(|e| Error::new(format!("rename {} {}: {}", src, dst, tools::errno_text(e.raw_os_error().unwrap_or(0)))))
}

fn new_http_request(env: &WorkerEnv, method: &str, rel: &Action) -> Result<Request> {
    let mut href = rel.href.clone();
    if cfg().git().bool("lfs.transfer.enablehrefrewrite", false) {
        href = endpoint::new_endpoint(env.dir.as_str(), &rel.href).url;
    }
    if !(href.starts_with("http://") || href.starts_with("https://")) {
        let frag = href.split('?').next().unwrap_or("");
        return Err(Error::new(format!("missing protocol: {}", tools::quote(frag))));
    }
    let mut req = Request::new(method, &href);
    for (k, v) in &rel.header {
        req.header.set(k, v);
    }
    Ok(req)
}

fn do_http(env: &WorkerEnv, t: &Transfer, req: &mut Request, hooks: &mut Hooks) -> lfsapi::Outcome {
    if t.authenticated {
        return http::client().do_(req, hooks);
    }
    let ep = req.url.split(t.oid.as_str()).next().unwrap_or("").to_string();
    lfsapi::do_with_auth_no_retry(&env.remote, endpoint::access_for(&ep), req, hooks)
}

fn basic_download(env: &WorkerEnv, t: &Transfer, auth_ok: &mut dyn FnMut(), zstd_init: &mut bool) -> Result<()> {
    let dir = incomplete_dir();
    let (f, tmp) = temp_file_in(&dir, &t.oid)?;
    drop(f);
    let part = format!("{dir}/{}.part", t.oid);
    let _ = std::fs::rename(&part, &tmp);
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&tmp).map_err(|e| tools::path_err("open", &tmp, &e))?;
    let mut h = Sha256::new();
    let mut from = std::io::copy(&mut f, &mut HashWriter(&mut h)).map_err(Error::from)? as i64;
    let mut hash = Some(h);
    if from > 0 {
        if from < t.size - 1 {
            crate::trace!("xfer: Attempting to resume download of {} from byte {}", tools::quote(&t.oid), from);
        } else {
            f.set_len(0)?;
            from = 0;
            hash = None;
        }
    }
    let r = download(env, t, auth_ok, &mut f, &tmp, from, hash, zstd_init);
    if r.is_err() {
        drop(f);
        let _ = std::fs::rename(&tmp, &part);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.update(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn download(env: &WorkerEnv, t: &Transfer, auth_ok: &mut dyn FnMut(), f: &mut std::fs::File, tmp: &str, mut from: i64, hash: Option<Sha256>, zstd_init: &mut bool) -> Result<()> {
    use std::io::Seek;
    let Some(rel) = t.rel("download")? else {
        return Err(Error::new(format!("Object {} not found on the server.", t.oid)));
    };
    let mut req = new_http_request(env, "GET", &rel)?;
    if from > 0 {
        req.header.set("Range", &format!("bytes={}-{}", from, t.size - 1));
    } else if let Some(enc) = crate::config::url_get("lfs.transfer", &rel.href, "httpdownloadencoding").filter(|e| !e.is_empty()) {
        match enc.as_str() {
            "gzip" => {}
            "zstd" => req.header.set("Accept-Encoding", "zstd"),
            _ => return Err(Error::new(format!("unsupported lfs.transfer.httpDownloadEncoding value {}: must be \"gzip\" or \"zstd\"", tools::quote(&enc)))),
        }
    }
    req.stats_key = Some("lfs.data.download".into());
    // The body goes straight to the file (or, zstd-encoded, to a side file first).
    f.seek(std::io::SeekFrom::Start(from as u64))?;
    let mut hasher = hash.unwrap_or_default();
    let mut written: i64 = 0;
    let mut checked = false;
    let mut range_failed: Option<String> = None;
    let mut zstd_buf: Option<Vec<u8>> = None;
    let start_from = from;
    let (res, err) = {
        let mut sink = |res: &http::Response, chunk: &[u8]| -> std::result::Result<(), String> {
            if !checked {
                checked = true;
                if start_from > 0 {
                    let ok = res.status == 206 && content_range_start(&res.header.get("Content-Range")) == Some(start_from);
                    if !ok {
                        range_failed = Some(range_fail_reason(res, start_from));
                        if res.status != 200 {
                            return Err("range".into());
                        }
                        // A whole body instead: start again.
                        f.set_len(0).map_err(|e| e.to_string())?;
                        f.seek(std::io::SeekFrom::Start(0)).map_err(|e| e.to_string())?;
                        hasher = Sha256::new();
                    }
                }
                if res.header.get("Content-Encoding").eq_ignore_ascii_case("zstd") {
                    zstd_buf = Some(vec![]);
                }
            }
            if let Some(z) = zstd_buf.as_mut() {
                z.extend_from_slice(chunk);
                return Ok(());
            }
            f.write_all(chunk).map_err(|e| format!("cannot write data to temporary file {}: {}", tools::quote(tmp), tools::io_err(&e)))?;
            hasher.update(chunk);
            written += chunk.len() as i64;
            let base = if range_failed.is_some() { 0 } else { start_from };
            (env.cb)(&t.name, t.size, written + base, chunk.len() as i64);
            Ok(())
        };
        let mut hooks = Hooks { sink: Some(&mut sink), ..Default::default() };
        make_request(env, t, &mut req, &mut hooks)
    };
    // A zstd-encoded body: its complete frames decoded (also when the response broke off,
    // so that a retry can resume from them).
    if let Some(z) = zstd_buf.take() {
        if !*zstd_init {
            *zstd_init = true;
            crate::trace!("http: initialized zstd decoder");
        }
        crate::trace!("http: decompressing zstd-encoded response");
        let (out, derr) = zstd_decode(&z);
        f.write_all(&out).map_err(|e| Error::new(tools::io_err(&e)).wrap(format!("cannot write data to temporary file {}", tools::quote(tmp))))?;
        hasher.update(&out);
        written += out.len() as i64;
        if !out.is_empty() {
            (env.cb)(&t.name, t.size, written + from, out.len() as i64);
        }
        if let (Some(e), None) = (derr, &err) {
            return Err(Error::new(e).wrap(format!("cannot write data to temporary file {}", tools::quote(tmp))));
        }
    }
    if let Some(reason) = &range_failed {
        crate::trace!("xfer: failed to resume download for {} from byte {}: {}. Re-downloading from start", tools::quote(&t.oid), from, reason);
        if res.as_ref().is_some_and(|r| r.status != 200) || err.is_some() {
            f.set_len(0)?;
            f.seek(std::io::SeekFrom::Start(0))?;
            return download(env, t, auth_ok, f, tmp, 0, None, zstd_init);
        }
        from = 0;
    } else if from > 0 && err.is_none() {
        crate::trace!("xfer: server accepted resume download request: {} from byte {}", tools::quote(&t.oid), from);
        advance_progress(env, t, from);
    }
    if let Some(e) = err {
        let Some(res) = res else { return Err(e.retriable()) };
        if from > 0 && res.status == 416 {
            crate::trace!("xfer: server rejected resume download request for {} from byte {}; re-downloading from start", tools::quote(&t.oid), from);
            f.set_len(0)?;
            f.seek(std::io::SeekFrom::Start(0))?;
            return download(env, t, auth_ok, f, tmp, 0, None, zstd_init);
        }
        if res.status == 429 {
            if let Some(l) = e.clone().retriable_later(&res.header.get("Retry-After")) {
                return Err(l);
            }
        }
        return Err(e.retriable());
    }
    auth_ok();
    let actual = tools::hex(&hasher.finalize());
    if actual != t.oid {
        return Err(Error::new(format!("expected OID {}, got {} after {} bytes written", t.oid, actual, written)));
    }
    f.flush()?;
    let r = rename_copy_permissions(tmp, &t.path);
    if std::fs::metadata(&t.path).is_ok() {
        return Ok(());
    }
    r
}

fn content_range_start(h: &str) -> Option<i64> {
    let re = regex::Regex::new(r"bytes (\d+)\-.*").unwrap();
    re.captures(h).and_then(|m| m[1].parse().ok())
}

fn range_fail_reason(res: &http::Response, from: i64) -> String {
    if res.status != 206 {
        return format!("expected status code 206, received {}", res.status);
    }
    let h = res.header.get("Content-Range");
    if h.is_empty() {
        return "missing Content-Range header in response".into();
    }
    let re = regex::Regex::new(r"bytes (\d+)\-.*").unwrap();
    match re.captures(&h) {
        Some(m) => format!("Content-Range start byte incorrect: {} expected {}", &m[1], from),
        None => format!("badly formatted Content-Range header: {}", tools::quote(&h)),
    }
}

/// A zstd stream decoded: what came out, and why it stopped early (a stream cut short still
/// gives the frames before the cut).
fn zstd_decode(data: &[u8]) -> (Vec<u8>, Option<String>) {
    use structured_zstd::decoding::StreamingDecoder;
    let mut out = vec![];
    let mut input: &[u8] = data;
    let mut d = match StreamingDecoder::new(&mut input) {
        Ok(d) => d,
        Err(e) => return (out, Some(e.to_string())),
    };
    // read_to_end keeps what was read before an error.
    match d.read_to_end(&mut out) {
        Ok(_) => (out, None),
        Err(e) => (out, Some(e.to_string())),
    }
}

/// makeRequest: sent again (without retry count) after an authentication error.
fn make_request(env: &WorkerEnv, t: &Transfer, req: &mut Request, hooks: &mut Hooks) -> lfsapi::Outcome {
    loop {
        let (res, err) = do_http(env, t, req, hooks);
        if err.as_ref().is_some_and(|e| e.is(Kind::Auth)) && !req.header.has("Authorization") {
            continue;
        }
        return (res, err);
    }
}

/// http.DetectContentType's answer for the first bytes (the common types).
fn detect_content_type(b: &[u8]) -> &'static str {
    let starts = |p: &[u8]| b.starts_with(p);
    let first_non_ws = b.iter().position(|c| !b"\t\n\x0c\r ".contains(c)).unwrap_or(b.len());
    let t = &b[first_non_ws..];
    let ci = |p: &[u8]| t.len() >= p.len() && t[..p.len()].eq_ignore_ascii_case(p);
    if ci(b"<!DOCTYPE HTML") || ci(b"<HTML") || ci(b"<HEAD") || ci(b"<SCRIPT") || ci(b"<IFRAME") || ci(b"<H1") || ci(b"<DIV") || ci(b"<FONT") || ci(b"<TABLE") || ci(b"<A") || ci(b"<STYLE") || ci(b"<TITLE") || ci(b"<B") || ci(b"<BODY") || ci(b"<BR") || ci(b"<P") || ci(b"<!--") {
        return "text/html; charset=utf-8";
    }
    if t.starts_with(b"<?xml") {
        return "text/xml; charset=utf-8";
    }
    if starts(b"%PDF-") {
        return "application/pdf";
    }
    if starts(b"%!PS-Adobe-") {
        return "application/postscript";
    }
    if starts(b"\xFE\xFF") || starts(b"\xFF\xFE") {
        return "text/plain; charset=utf-16";
    }
    if starts(b"\xEF\xBB\xBF") {
        return "text/plain; charset=utf-8";
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return "image/gif";
    }
    if starts(b"\x89PNG\x0D\x0A\x1A\x0A") {
        return "image/png";
    }
    if starts(b"\xFF\xD8\xFF") {
        return "image/jpeg";
    }
    if starts(b"BM") {
        return "image/bmp";
    }
    if starts(b"\x1F\x8B\x08") {
        return "application/x-gzip";
    }
    if starts(b"PK\x03\x04") {
        return "application/zip";
    }
    if starts(b"Rar!\x1A\x07\x00") || starts(b"Rar!\x1A\x07\x01\x00") {
        return "application/x-rar-compressed";
    }
    if starts(b"\x00\x61\x73\x6D") {
        return "application/wasm";
    }
    if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return "image/webp";
    }
    if b.len() >= 8 && &b[4..8] == b"ftyp" {
        return "video/mp4";
    }
    if starts(b"OggS\x00") {
        return "application/ogg";
    }
    if starts(b"ID3") {
        return "audio/mpeg";
    }
    if b.iter().any(|&c| c <= 0x08 || c == 0x0B || (0x0E..=0x1A).contains(&c) || (0x1C..=0x1F).contains(&c)) {
        return "application/octet-stream";
    }
    "text/plain; charset=utf-8"
}

fn set_content_type(req: &mut Request, path: &str) -> Result<()> {
    if req.header.has("Content-Type") && !req.header.get("Content-Type").is_empty() {
        return Ok(());
    }
    let disabled = !crate::config::url_bool("lfs", &req.url, "contenttype", true);
    let mut ct = String::new();
    if !disabled {
        let mut buf = vec![0u8; 512];
        let mut f = std::fs::File::open(path).map_err(|e| tools::path_err("open", path, &e).wrap("content type detection error"))?;
        let mut n = 0;
        while n < 512 {
            match f.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) => return Err(Error::new(tools::io_err(&e)).wrap("content type detection error")),
            }
        }
        ct = detect_content_type(&buf[..n]).to_string();
    }
    if ct.is_empty() {
        ct = "application/octet-stream".into();
    }
    req.header.set("Content-Type", &ct);
    Ok(())
}

fn basic_upload(env: &WorkerEnv, t: &Transfer, auth_ok: &mut dyn FnMut()) -> Result<()> {
    let Some(rel) = t.rel("upload")? else {
        return Err(Error::new(format!("No upload action for object: {}", t.oid)));
    };
    let mut req = new_http_request(env, "PUT", &rel)?;
    if !req.header.get("Transfer-Encoding").eq_ignore_ascii_case("chunked") {
        req.header.set("Content-Length", &t.size.to_string());
    }
    if let Err(e) = std::fs::metadata(&t.path) {
        return Err(tools::path_err("open", &t.path, &e).wrap("basic upload"));
    }
    set_content_type(&mut req, &t.path)?;
    req.body = Some(http::Body::File { path: t.path.clone(), offset: 0, len: t.size as u64 });
    req.stats_key = Some("lfs.data.upload".into());
    let mut read: i64 = 0;
    let mut first = true;
    let (res, err) = {
        let mut prog = |n: i64| {
            read += n;
            (env.cb)(&t.name, t.size, read, n);
        };
        let mut start = || {
            if first {
                first = false;
                auth_ok();
            }
        };
        let mut hooks = Hooks { progress: Some(&mut prog), on_start: Some(&mut start), ..Default::default() };
        let (res, err) = do_http(env, t, &mut req, &mut hooks);
        if err.as_ref().is_some_and(|e| e.is(Kind::Auth)) && !req.header.has("Authorization") {
            // makeRequest: sent again, the body without progress callbacks.
            make_request(env, t, &mut req, &mut Hooks::default())
        } else {
            (res, err)
        }
    };
    if let Some(e) = err {
        if e.is(Kind::Unprocessable) {
            return Err(e);
        }
        if read > 0 {
            (env.cb)(&t.name, t.size, 0, -read);
        }
        let Some(res) = res else { return Err(e.retriable()) };
        if res.status == 429 {
            if let Some(l) = e.clone().retriable_later(&res.header.get("Retry-After")) {
                return Err(l);
            }
        }
        return Err(e.retriable());
    }
    let res = res.unwrap();
    if res.status == 403 {
        return Err(Error::new(format!("Received status {}", res.status)).retriable());
    }
    if res.status > 299 {
        return Err(Error::new(format!("Invalid status for {} {}: {}", req.method, req.url_no_query(), res.status)));
    }
    verify_upload(&env.remote, t)
}

fn tus_upload(env: &WorkerEnv, t: &Transfer, auth_ok: &mut dyn FnMut()) -> Result<()> {
    let Some(rel) = t.rel("upload")? else {
        return Err(Error::new(format!("No upload action for object: {}", t.oid)));
    };
    env.trace(|| format!("xfer: sending tus.io HEAD request for {}", tools::quote(&t.oid)));
    let mut req = new_http_request(env, "HEAD", &rel)?;
    req.header.set("Tus-Resumable", "1.0.0");
    let (res, err) = do_http(env, t, &mut req, &mut Hooks::default());
    if let Some(e) = err {
        return Err(e.retriable());
    }
    let res = res.unwrap();
    let off = res.header.get("Upload-Offset");
    if off.is_empty() {
        return Err(Error::new(format!("missing Upload-Offset header from tus.io HEAD response at {}, contact server admin", tools::quote(&rel.href))));
    }
    let offset = match off.parse::<i64>() {
        Ok(o) if o >= 0 => o,
        _ => return Err(Error::new(format!("invalid Upload-Offset value {} in response from tus.io HEAD at {}, contact server admin", tools::quote(&off), tools::quote(&rel.href)))),
    };
    if offset >= t.size {
        env.trace(|| format!("xfer: tus.io HEAD offset {} indicates {} is already fully uploaded, skipping", offset, tools::quote(&t.oid)));
        advance_progress(env, t, t.size);
        return Ok(());
    }
    if let Err(e) = std::fs::metadata(&t.path) {
        return Err(tools::path_err("open", &t.path, &e).wrap("tus.io upload"));
    }
    if offset == 0 {
        env.trace(|| format!("xfer: tus.io uploading {} from start", tools::quote(&t.oid)));
    } else {
        env.trace(|| format!("xfer: tus.io resuming upload {} from {}", tools::quote(&t.oid), offset));
        advance_progress(env, t, offset);
    }
    env.trace(|| format!("xfer: sending tus.io PATCH request for {}", tools::quote(&t.oid)));
    let mut req = new_http_request(env, "PATCH", &rel)?;
    req.header.set("Tus-Resumable", "1.0.0");
    req.header.set("Upload-Offset", &offset.to_string());
    req.header.set("Content-Type", "application/offset+octet-stream");
    req.header.set("Content-Length", &(t.size - offset).to_string());
    req.body = Some(http::Body::File { path: t.path.clone(), offset: offset as u64, len: (t.size - offset) as u64 });
    req.stats_key = Some("lfs.data.upload".into());
    let mut read = offset;
    let (res, err) = {
        let mut prog = |n: i64| {
            read += n;
            (env.cb)(&t.name, t.size, read, n);
        };
        let mut start = || auth_ok();
        let mut hooks = Hooks { progress: Some(&mut prog), on_start: Some(&mut start), ..Default::default() };
        do_http(env, t, &mut req, &mut hooks)
    };
    if let Some(e) = err {
        return Err(e.retriable());
    }
    let res = res.unwrap();
    if res.status == 403 {
        return Err(Error::new(format!("Received status {}", res.status)).retriable());
    }
    if res.status > 299 {
        return Err(Error::new(format!("Invalid status for {} {}: {}", req.method, req.url_no_query(), res.status)));
    }
    verify_upload(&env.remote, t)
}

/// verifyUpload: POST to the verify action, if there is one.
pub fn verify_upload(remote: &str, t: &Transfer) -> Result<()> {
    let Some(action) = action_get(&t.actions, "verify")? else { return Ok(()) };
    let mut req = Request::new("POST", &action.href).with_json(&serde_json::json!({"oid": t.oid, "size": t.size}));
    req.header.set("Content-Type", http::MEDIA_TYPE);
    req.header.set("Accept", http::MEDIA_TYPE);
    for (k, v) in &action.header {
        req.header.set(k, v);
    }
    let mv = cfg().git().int("lfs.transfer.maxverifies", 3).max(3);
    req.stats_key = Some("lfs.verify".into());
    let mut last = None;
    for i in 1..=mv {
        crate::trace!("tq: verify {} attempt #{} (max: {})", &t.oid[..7.min(t.oid.len())], i, mv);
        let mut r = req.clone();
        let (_, err) = if t.authenticated {
            http::client().do_(&mut r, &mut Hooks::default())
        } else {
            lfsapi::do_with_auth(remote, endpoint::access_for(&action.href), &mut r, &mut Hooks::default())
        };
        match err {
            Some(e) => {
                crate::trace!("tq: verify err: {}", e);
                last = Some(e);
            }
            None => return Ok(()),
        }
    }
    Err(last.unwrap())
}

// Custom transfer adapters (and the standalone file adapter, `git lfs standalone-file`).

struct CustomProcess {
    n: usize,
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
}

impl CustomProcess {
    fn start(env: &WorkerEnv, n: usize, path: &str, args: &str, concurrent: bool) -> Result<CustomProcess> {
        let script = format!("{} {}", crate::subprocess::shell_quote_single(path), args);
        let mut child = crate::subprocess::command("sh", &["-c", &script])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Error::new(format!("failed to start custom transfer command {} remote: {}", tools::quote(path), tools::io_err(&e))))?;
        let stderr = child.stderr.take().unwrap();
        let pname = std::path::Path::new(path).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let stderr_thread = std::thread::spawn(move || {
            let r = std::io::BufReader::new(stderr);
            use std::io::BufRead;
            for line in r.lines().map_while(|l| l.ok()) {
                if !line.trim().is_empty() {
                    crate::trace!("xfer[{}]: {}", pname, line.trim());
                }
            }
        });
        let mut p = CustomProcess {
            n,
            stdin: child.stdin.take(),
            stdout: std::io::BufReader::new(child.stdout.take().unwrap()),
            child,
            stderr_thread: Some(stderr_thread),
        };
        let op = if env.dir == Direction::Download { "download" } else { "upload" };
        let init = serde_json::json!({"event": "init", "operation": op, "remote": env.remote, "concurrent": concurrent, "concurrenttransfers": env.original_concurrency});
        let resp = match p.exchange(env, &init) {
            Ok(r) => r,
            Err(e) => {
                p.abort(env);
                return Err(e);
            }
        };
        if let Some(e) = resp.get("error").filter(|e| !e.is_null()) {
            p.abort(env);
            return Err(Error::new(format!("error initializing custom adapter {} worker {}: {}", tools::quote(&env.name), n, object_error_text(e))));
        }
        env.trace(|| format!("xfer: started custom adapter process {} for worker {} OK", tools::quote(path), n));
        Ok(p)
    }

    fn send(&mut self, env: &WorkerEnv, v: &serde_json::Value) -> Result<()> {
        let s = serde_json::to_string(v).unwrap();
        env.trace(|| format!("xfer: Custom adapter worker {} sending message: {}", self.n, s));
        let w = self.stdin.as_mut().ok_or_else(|| Error::new("write |1: file already closed"))?;
        w.write_all(format!("{s}\n").as_bytes()).map_err(|e| Error::new(format!("write |1: {}", tools::io_err(&e))))?;
        w.flush().map_err(Error::from)
    }

    fn read(&mut self, env: &WorkerEnv) -> Result<serde_json::Value> {
        use std::io::BufRead;
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).map_err(Error::from)?;
        if n == 0 {
            return Err(Error::new("EOF"));
        }
        env.trace(|| format!("xfer: Custom adapter worker {} received response: {}", self.n, line.trim()));
        serde_json::from_str(&line).map_err(|e| Error::new(e.to_string()))
    }

    fn exchange(&mut self, env: &WorkerEnv, v: &serde_json::Value) -> Result<serde_json::Value> {
        self.send(env, v)?;
        self.read(env)
    }

    fn abort(&mut self, env: &WorkerEnv) {
        env.trace(|| format!("xfer: Aborting worker process: {}", self.n));
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn object_error_text(e: &serde_json::Value) -> String {
    format!("[{}] {}", e.get("code").and_then(|c| c.as_i64()).unwrap_or(0), e.get("message").and_then(|m| m.as_str()).unwrap_or(""))
}

fn worker_ending_custom(env: &WorkerEnv, mut p: CustomProcess) {
    env.trace(|| format!("xfer: Shutting down adapter worker {}", p.n));
    let r = p.send(env, &serde_json::json!({"event": "terminate"}));
    p.stdin.take();
    let waited = match r {
        Ok(()) => p.child.wait().map(|s| s.success()).unwrap_or(false),
        Err(_) => false,
    };
    if let Some(t) = p.stderr_thread.take() {
        let _ = t.join();
    }
    if !waited {
        crate::trace!("xfer: error finishing up custom transfer process worker {}, aborting", p.n);
        p.abort(env);
    }
}

fn custom_transfer(env: &WorkerEnv, p: &mut CustomProcess, t: &Transfer, standalone: bool, auth_ok: &mut dyn FnMut()) -> Result<()> {
    let op = if env.dir == Direction::Download { "download" } else { "upload" };
    let rel = t.rel(op)?;
    if rel.is_none() && !standalone {
        return Err(Error::new(format!("Object {} not found on the server.", t.oid)));
    }
    let action = rel.as_ref().map(|a| a.to_go_json()).unwrap_or(serde_json::Value::Null);
    let req = if env.dir == Direction::Upload {
        let mut m = serde_json::json!({"event": "upload", "oid": t.oid, "size": t.size});
        if !t.path.is_empty() {
            m["path"] = t.path.clone().into();
        }
        m["action"] = action;
        m
    } else {
        serde_json::json!({"event": "download", "oid": t.oid, "size": t.size, "action": action})
    };
    p.send(env, &req)?;
    let mut auth_called = false;
    loop {
        let resp = p.read(env)?;
        let event = resp.get("event").and_then(|e| e.as_str()).unwrap_or("");
        let oid = resp.get("oid").and_then(|e| e.as_str()).unwrap_or("");
        let was_auth_ok;
        let mut complete = false;
        match event {
            "progress" => {
                if oid != t.oid {
                    return Err(Error::new(format!("unexpected OID {} in response, expecting {}", tools::quote(oid), tools::quote(&t.oid))));
                }
                let so_far = resp.get("bytesSoFar").and_then(|v| v.as_i64()).unwrap_or(0);
                let since = resp.get("bytesSinceLast").and_then(|v| v.as_i64()).unwrap_or(0);
                progress(env, t, so_far, since);
                was_auth_ok = so_far > 0;
            }
            "complete" => {
                if oid != t.oid {
                    return Err(Error::new(format!("unexpected OID {} in response, expecting {}", tools::quote(oid), tools::quote(&t.oid))));
                }
                if let Some(e) = resp.get("error").filter(|e| !e.is_null()) {
                    return Err(Error::new(format!("error transferring {}: {}", tools::quote(&t.oid), object_error_text(e))));
                }
                if env.dir == Direction::Download {
                    let path = resp.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    verify_file_hash(&t.oid, path).map_err(|e| Error::new(format!("downloaded file failed checks: {e}")))?;
                    rename_copy_permissions(path, &t.path).map_err(|e| Error::new(format!("failed to copy downloaded file: {e}")))?;
                } else {
                    verify_upload(&env.remote, t)?;
                }
                was_auth_ok = true;
                complete = true;
            }
            _ => return Err(Error::new(format!("invalid message {} from custom adapter {}", tools::quote(event), tools::quote(&env.name)))),
        }
        if was_auth_ok && !auth_called {
            auth_ok();
            auth_called = true;
        }
        if complete {
            return Ok(());
        }
    }
}

/// tools.VerifyFileHash.
pub fn verify_file_hash(oid: &str, path: &str) -> Result<()> {
    let mut f = std::fs::File::open(path).map_err(|e| tools::path_err("open", path, &e))?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut HashWriter(&mut h))?;
    let got = tools::hex(&h.finalize());
    if got != oid {
        return Err(Error::new(format!("file {} has an invalid hash {}, expected {}", tools::quote(path), got, oid)));
    }
    Ok(())
}

// The pure SSH adapter.

fn ssh_args(t: &Transfer, action: &str) -> Vec<String> {
    let Some(a) = t.actions.get(action) else { return vec![] };
    let mut v = vec![format!("size={}", t.size)];
    if !a.id.is_empty() {
        v.push(format!("id={}", a.id));
    }
    if !a.token.is_empty() {
        v.push(format!("token={}", a.token));
    }
    v
}

fn ssh_download(env: &WorkerEnv, s: &SshTransfer, n: usize, t: &Transfer) -> Result<()> {
    if t.rel("download")?.is_none() {
        return Err(Error::new(format!("No download action for object: {}", t.oid)));
    }
    let dir = incomplete_dir();
    let (mut f, tmp) = temp_file_in(&dir, &t.oid)?;
    let r = (|| {
        let args = ssh_args(t, "download");
        let conn = s.connection(n)?;
        let (status, args, data) = {
            let mut c = conn.lock().unwrap();
            c.send_message(&format!("get-object {}", t.oid), &args)?;
            c.read_status_with_data()?
        };
        if !(200..=299).contains(&status) {
            let text = String::from_utf8_lossy(&data[..data.len().min(1024)]).into_owned();
            return Err(Error::new(format!("got status {} when fetching OID {}: {}", status, t.oid, text)).retriable());
        }
        let mut seen = false;
        for a in &args {
            if let Some(v) = a.strip_prefix("size=") {
                if seen {
                    return Err(Error::protocol("unexpected size argument", None));
                }
                match v.parse::<i64>() {
                    Ok(x) if x >= 0 => {}
                    _ => return Err(Error::protocol(&format!("expected valid size, got {}", tools::quote(v)), None)),
                }
                seen = true;
            }
        }
        if !seen {
            return Err(Error::protocol("no size argument seen", None));
        }
        let mut h = Sha256::new();
        let mut written = 0i64;
        for c in data.chunks(32768) {
            f.write_all(c).map_err(|e| Error::new(tools::io_err(&e)).wrap(format!("cannot write data to temporary file {}", tools::quote(&tmp))))?;
            h.update(c);
            written += c.len() as i64;
            progress(env, t, written, c.len() as i64);
        }
        let actual = tools::hex(&h.finalize());
        if actual != t.oid {
            return Err(Error::new(format!("expected OID {}, got {} after {} bytes written", t.oid, actual, written)));
        }
        let r = rename_copy_permissions(&tmp, &t.path);
        if std::fs::metadata(&t.path).is_ok() {
            return Ok(());
        }
        r
    })();
    let _ = std::fs::remove_file(&tmp);
    r
}

fn ssh_upload(env: &WorkerEnv, s: &SshTransfer, n: usize, t: &Transfer) -> Result<()> {
    if t.rel("upload")?.is_none() {
        return Err(Error::new(format!("No upload action for object: {}", t.oid)));
    }
    let f = std::fs::File::open(&t.path).map_err(|e| tools::path_err("open", &t.path, &e).wrap("SSH upload"))?;
    let args = ssh_args(t, "upload");
    let conn = s.connection(n)?;
    let (status, lines) = {
        let mut c = conn.lock().unwrap();
        let mut read = 0i64;
        let mut r = CallbackReader { r: f, cb: &mut |k: i64| {
            read += k;
            progress(env, t, read, k);
        } };
        c.send_message_with_data(&format!("put-object {}", t.oid), &args, &mut r)?;
        let (st, _, lines) = c.read_status_with_lines()?;
        (st, lines)
    };
    if !(200..=299).contains(&status) {
        if status == 403 {
            return Err(Error::new(format!("Received status {status}")).retriable());
        }
        if status == 429 {
            return Err(Error::new(format!("got status {} when uploading OID {}", status, t.oid)).retriable());
        }
        return Err(Error::new(match lines.first() {
            Some(l) => format!("got status {} when uploading OID {}: {}", status, t.oid, l),
            None => format!("got status {} when uploading OID {}", status, t.oid),
        }));
    }
    let args = ssh_args(t, "upload");
    let (status, _, lines) = {
        let mut c = conn.lock().unwrap();
        c.send_message(&format!("verify-object {}", t.oid), &args)?;
        c.read_status_with_lines()?
    };
    if !(200..=299).contains(&status) {
        return Err(Error::new(match lines.first() {
            Some(l) => format!("got status {} when verifying upload OID {}: {}", status, t.oid, l),
            None => format!("got status {} when verifying upload OID {}", status, t.oid),
        }));
    }
    Ok(())
}

struct CallbackReader<'a, R: Read> {
    r: R,
    cb: &'a mut dyn FnMut(i64),
}

impl<R: Read> Read for CallbackReader<'_, R> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let n = self.r.read(b)?;
        if n > 0 {
            (self.cb)(n as i64);
        }
        Ok(n)
    }
}

// The queue.

#[derive(Clone, Debug)]
struct ObjectTuple {
    name: String,
    path: String,
    oid: String,
    size: i64,
    missing: bool,
    ready_time: Option<Instant>,
    retry_later_time: Option<Instant>,
}

struct Objects {
    completed: bool,
    objects: Vec<ObjectTuple>,
}

pub type Watcher = Box<dyn FnMut(&Transfer)>;

pub struct TransferQueue {
    direction: Direction,
    remote: String,
    remote_ref: Option<Ref>,
    dry_run: bool,
    pub batch_size: usize,
    meter: Option<Arc<Meter>>,
    cb: Option<ProgressCb>,
    manifest: Arc<Manifest>,
    transfers: HashMap<String, Objects>,
    incoming: Vec<ObjectTuple>,
    pending: Vec<ObjectTuple>,
    errors: Vec<Error>,
    watchers: Vec<Watcher>,
    adapter: Option<Adapter>,
    adapter_name: String,
    retry_count: HashMap<String, i64>,
    aborted: bool,
    unsupported_content_type: bool,
    counted: AtomicI64,
}

pub struct Options {
    pub dry_run: bool,
    pub meter: Option<Arc<Meter>>,
    pub remote_ref: Option<Ref>,
    pub batch_size: i64,
    pub cb: Option<ProgressCb>,
}

impl Default for Options {
    fn default() -> Self {
        Options { dry_run: false, meter: None, remote_ref: None, batch_size: 0, cb: None }
    }
}

impl TransferQueue {
    pub fn new(direction: Direction, manifest: Arc<Manifest>, remote: &str, o: Options) -> TransferQueue {
        let batch_size = if o.batch_size <= 0 { 100 } else { o.batch_size as usize };
        if let Some(m) = &o.meter {
            m.set_direction(direction);
        }
        crate::trace!("tq: running as batched queue, batch size of {}", batch_size);
        TransferQueue {
            direction,
            remote: remote.to_string(),
            remote_ref: o.remote_ref,
            dry_run: o.dry_run,
            batch_size,
            meter: o.meter,
            cb: o.cb,
            manifest,
            transfers: HashMap::new(),
            incoming: vec![],
            pending: vec![],
            errors: vec![],
            watchers: vec![],
            adapter: None,
            adapter_name: String::new(),
            retry_count: HashMap::new(),
            aborted: false,
            unsupported_content_type: false,
            counted: AtomicI64::new(0),
        }
    }

    pub fn watch(&mut self, w: Watcher) {
        self.watchers.push(w);
    }

    fn notify(&mut self, t: &Transfer) {
        for w in self.watchers.iter_mut() {
            w(t);
        }
    }

    pub fn add_error(&mut self, e: Error) {
        self.manifest.upgrade();
        self.errors.push(e);
    }

    pub fn add(&mut self, name: &str, path: &str, oid: &str, size: i64, missing: bool) {
        self.manifest.upgrade();
        let t = ObjectTuple { name: name.into(), path: path.into(), oid: oid.into(), size, missing, ready_time: None, retry_later_time: None };
        match self.transfers.get_mut(oid) {
            Some(objs) => {
                objs.objects.push(t.clone());
                if objs.completed {
                    let tr = Transfer { name: t.name.clone(), path: t.path.clone(), oid: t.oid.clone(), size: t.size, ..Default::default() };
                    self.notify(&tr);
                }
                crate::trace!("already transferring {}, skipping duplicate", tools::quote(oid));
                return;
            }
            None => {
                self.transfers.insert(oid.to_string(), Objects { completed: false, objects: vec![t.clone()] });
            }
        }
        self.incoming.push(t);
        if self.incoming.len() >= self.batch_size && !self.aborted {
            let next: Vec<ObjectTuple> = self.incoming.drain(..).collect();
            self.process(next);
        }
    }

    fn meter<F: FnOnce(&Meter)>(&self, f: F) {
        if let Some(m) = &self.meter {
            f(m);
        }
    }

    fn skip(&self, size: i64) {
        self.meter(|m| m.skip(size));
    }

    fn process(&mut self, mut next: Vec<ObjectTuple>) {
        next.sort_by(|a, b| b.size.cmp(&a.size));
        match self.enqueue_and_collect_retries_for(next) {
            Ok(retries) => self.pending.extend(retries),
            Err((retries, e, retriable)) => {
                self.pending.extend(retries);
                self.errors.push(e);
                if !retriable {
                    self.aborted = true;
                }
            }
        }
    }

    fn can_retry_count(&self, oid: &str) -> bool {
        let c = self.retry_count.get(oid).copied().unwrap_or(0);
        let max = self.manifest.upgrade().max_retries;
        if c >= max {
            crate::trace!("tq: refusing to retry {}, too many retries ({})", tools::quote(oid), c);
            return false;
        }
        true
    }

    fn can_retry_object(&self, oid: &str, e: &Error) -> bool {
        self.can_retry_count(oid) && e.is(Kind::Retriable)
    }

    fn can_retry_object_later(&self, oid: &str, e: &Error) -> Option<Instant> {
        if !self.can_retry_count(oid) {
            return None;
        }
        let at = e.retry_later_at()?;
        let now = SystemTime::now();
        let delay = at.duration_since(now).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let max = self.manifest.upgrade().max_retry_time;
        if delay > max as f64 {
            crate::trace!("tq: refusing to retry {}, retry after {:.0}s exceeds maximum {}s", tools::quote(oid), delay, max);
            return None;
        }
        Some(Instant::now() + Duration::from_secs_f64(delay))
    }

    fn ready_time(&self, oid: &str) -> Option<Instant> {
        let count = self.retry_count.get(oid).copied().unwrap_or(0);
        if count < 1 {
            return None;
        }
        let max_ms = 1000 * self.manifest.upgrade().max_retry_delay as u64;
        let mut delay = 250u64.checked_shl((count - 1) as u32).unwrap_or(0);
        if delay == 0 || delay > max_ms {
            delay = max_ms;
        }
        Some(Instant::now() + Duration::from_millis(delay))
    }

    fn enqueue_retry(&mut self, next: &mut Vec<ObjectTuple>, mut t: ObjectTuple, err: Option<&Error>, ready: Option<Instant>) {
        let count = {
            let c = self.retry_count.entry(t.oid.clone()).or_insert(0);
            *c += 1;
            *c
        };
        if let Some(rl) = t.retry_later_time.take() {
            t.ready_time = Some(rl);
        } else if let Some(r) = ready {
            t.ready_time = Some(r);
        } else {
            t.ready_time = self.ready_time(&t.oid);
        }
        let delay = t.ready_time.map(|r| r.saturating_duration_since(Instant::now()).as_secs_f64()).unwrap_or(0.0);
        let msg = err.map(|e| format!(": {e}")).unwrap_or_default();
        crate::trace!("tq: enqueue retry #{} after {:.2}s for {} (size: {}){}", count, delay, tools::quote(&t.oid), t.size, msg);
        next.push(t);
    }

    fn first(&self, oid: &str) -> Option<ObjectTuple> {
        self.transfers.get(oid).and_then(|o| o.objects.first().cloned())
    }

    #[allow(clippy::type_complexity)]
    fn enqueue_and_collect_retries_for(&mut self, batch_objs: Vec<ObjectTuple>) -> std::result::Result<Vec<ObjectTuple>, (Vec<ObjectTuple>, Error, bool)> {
        let mut next = vec![];
        crate::trace!("tq: sending batch of size {}", batch_objs.len());
        self.meter(|m| m.pause());
        let manifest = self.manifest.clone();
        let m = manifest.upgrade();
        let bres = if !m.standalone_agent.is_empty() {
            BatchResponse {
                objects: batch_objs.iter().map(|t| Transfer { oid: t.oid.clone(), size: t.size, path: t.path.clone(), ..Default::default() }).collect(),
                transfer: m.standalone_agent.clone(),
            }
        } else {
            let objs: Vec<Transfer> = batch_objs.iter().map(|t| Transfer { oid: t.oid.clone(), size: t.size, ..Default::default() }).collect();
            match batch(m, self.direction, &self.remote, self.remote_ref.as_ref(), &objs) {
                Ok(b) => b,
                Err(e) => {
                    let mut non_retriable = false;
                    for t in batch_objs {
                        if self.can_retry_object(&t.oid, &e) {
                            self.enqueue_retry(&mut next, t, Some(&e), None);
                        } else if let Some(r) = self.can_retry_object_later(&t.oid, &e) {
                            self.enqueue_retry(&mut next, t, Some(&e), Some(r));
                        } else {
                            non_retriable = true;
                        }
                    }
                    if non_retriable {
                        return Err((next, e.retriable(), true));
                    }
                    return Ok(next);
                }
            }
        };
        if bres.objects.is_empty() {
            return Ok(next);
        }
        if self.direction == Direction::Upload {
            for o in &bres.objects {
                if !o.actions.is_empty() {
                    if let Some(f) = self.first(&o.oid) {
                        if f.missing {
                            crate::trace!("tq: stopping batched queue, object {} missing locally and on remote", tools::quote(&o.oid));
                            return Err((vec![], object_missing_error(&f.name, &o.oid), false));
                        }
                    }
                }
            }
        }
        self.use_adapter(&bres.transfer);
        self.meter(|m| m.start());
        let standalone = !m.standalone_agent.is_empty();
        let mut to_transfer = vec![];
        for o in bres.objects {
            if let Some(oe) = &o.error {
                let e = Error::new(format!("[{}] {}", oe.code, oe.message)).wrap(format!("[{}] {}", o.oid, oe.message));
                self.errors.push(e);
                self.skip(o.size);
                continue;
            }
            let Some(first) = self.first(&o.oid) else {
                self.errors.push(Error::new(format!("[{}] The server returned an unknown OID.", o.oid)));
                self.skip(o.size);
                continue;
            };
            let mut tr = o.clone();
            tr.name = first.name.clone();
            tr.path = first.path.clone();
            tr.error = None;
            match tr.rel(self.direction.as_str()) {
                Err(e) => {
                    if self.can_retry_object(&tr.oid, &e) {
                        self.enqueue_retry(&mut next, first, Some(&e), None);
                    } else {
                        self.errors.push(Error::new(format!("[{}] {}", tr.name, e)));
                        self.skip(o.size);
                    }
                }
                Ok(None) if !standalone => self.skip(o.size),
                Ok(_) => {
                    self.meter(|m| m.start_transfer(&first.name));
                    to_transfer.push(tr);
                }
            }
        }
        for t in self.add_to_adapter(to_transfer) {
            self.enqueue_retry(&mut next, t, None, None);
        }
        Ok(next)
    }

    fn use_adapter(&mut self, name: &str) {
        let name = if name.is_empty() { "basic" } else { name };
        if self.adapter.is_some() {
            if self.adapter_name == name {
                return;
            }
            self.finish_adapter();
        }
        self.adapter_name = name.to_string();
    }

    fn finish_adapter(&mut self) {
        if let Some(a) = self.adapter.take() {
            a.end();
        }
    }

    fn ensure_adapter_begun(&mut self) -> Result<()> {
        if self.adapter.is_some() {
            return Ok(());
        }
        let manifest = self.manifest.clone();
        let m = manifest.upgrade();
        let (name, kind) = m.new_adapter_or_default(&self.adapter_name, self.direction);
        self.adapter_name = name.clone();
        crate::trace!("tq: starting transfer adapter {}", tools::quote(&name));
        let meter = self.meter.clone();
        let qcb = self.cb.clone();
        let dir = self.direction.as_str();
        let cb: ProgressCb = Arc::new(move |name: &str, total: i64, read: i64, current: i64| {
            if let Some(m) = &meter {
                m.transfer_bytes(dir, name, read, total, current);
            }
            if let Some(c) = &qcb {
                c(name, total, read, current);
            }
        });
        let a = Adapter::begin(&name, kind, self.direction, m.concurrent_transfers, &self.remote, cb)?;
        self.adapter = Some(a);
        Ok(())
    }

    /// The transfers through the adapter; those to try again.
    fn add_to_adapter(&mut self, pending: Vec<Transfer>) -> Vec<ObjectTuple> {
        let mut retries = vec![];
        if pending.is_empty() {
            return retries;
        }
        if let Err(e) = self.ensure_adapter_begun() {
            self.errors.push(e);
            for t in &pending {
                self.skip(t.size);
            }
            return retries;
        }
        let (present, missing) = self.partition(pending);
        for (t, e) in missing {
            self.handle_result(t, Some(e), &mut retries);
        }
        if self.dry_run {
            for t in present {
                self.handle_result(t, None, &mut retries);
            }
        } else {
            let n = present.len();
            let rx = self.adapter.as_ref().unwrap().add(present);
            for _ in 0..n {
                match rx.recv() {
                    Ok((t, e)) => self.handle_result(t, e, &mut retries),
                    Err(_) => break,
                }
            }
        }
        retries
    }

    fn partition(&self, ts: Vec<Transfer>) -> (Vec<Transfer>, Vec<(Transfer, Error)>) {
        if self.direction != Direction::Upload {
            return (ts, vec![]);
        }
        let mut present = vec![];
        let mut missing = vec![];
        for t in ts {
            let err = if t.size < 0 {
                Some(Error::new(format!("object {} has invalid size (got: {})", tools::quote(&t.oid), t.size)))
            } else {
                match std::fs::metadata(&t.path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(object_missing_error(&t.name, &t.oid)),
                    Err(e) => Some(tools::path_err("stat", &t.path, &e)),
                    Ok(m) if m.len() as i64 != t.size => Some(corrupt_object_error(&t.name, &t.oid)),
                    Ok(_) => None,
                }
            };
            match err {
                Some(e) => missing.push((t, e)),
                None => present.push(t),
            }
        }
        (present, missing)
    }

    fn handle_result(&mut self, t: Transfer, err: Option<Error>, retries: &mut Vec<ObjectTuple>) {
        let oid = t.oid.clone();
        match err {
            Some(e) => {
                if let Some(ready) = self.can_retry_object_later(&oid, &e) {
                    crate::trace!("tq: retrying object {} after {:.2}s", oid, ready.saturating_duration_since(Instant::now()).as_secs_f64());
                    match self.first(&oid) {
                        Some(mut f) => {
                            f.retry_later_time = Some(ready);
                            retries.push(f);
                        }
                        None => self.errors.push(e),
                    }
                } else if self.can_retry_object(&oid, &e) {
                    crate::trace!("tq: retrying object {}: {}", oid, e);
                    match self.first(&oid) {
                        Some(f) => retries.push(f),
                        None => self.errors.push(e),
                    }
                } else if e.is(Kind::Unprocessable) {
                    self.unsupported_content_type = true;
                } else {
                    self.errors.push(e);
                }
            }
            None => {
                let objs: Vec<ObjectTuple> = match self.transfers.get_mut(&oid) {
                    Some(o) => {
                        o.completed = true;
                        o.objects.clone()
                    }
                    None => vec![],
                };
                for o in objs {
                    let tr = Transfer { name: o.name, path: o.path, oid: o.oid, size: o.size, actions: t.actions.clone(), links: t.links.clone(), ..Default::default() };
                    self.notify(&tr);
                }
                self.meter(|m| m.finish_transfer(&t.name));
                self.counted.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Wait: everything added is transferred (retries included), then the adapter ends.
    pub fn wait(&mut self) {
        loop {
            if self.aborted {
                break;
            }
            let now = Instant::now();
            let mut next: Vec<ObjectTuple> = vec![];
            let mut rest = vec![];
            let all: Vec<ObjectTuple> = self.pending.drain(..).chain(self.incoming.drain(..)).collect();
            let mut min_wait: Option<Duration> = None;
            for t in all {
                match t.ready_time {
                    Some(r) if r > now => {
                        let w = r - now;
                        min_wait = Some(min_wait.map_or(w, |m: Duration| m.min(w)));
                        rest.push(t);
                    }
                    _ => next.push(t),
                }
            }
            if next.len() > self.batch_size {
                rest.extend(next.split_off(self.batch_size));
            }
            self.pending = rest;
            if next.is_empty() {
                if self.pending.is_empty() {
                    break;
                }
                let w = min_wait.unwrap_or_default();
                crate::trace!("tq: rate limited, waiting {} before retrying", go_duration(w));
                std::thread::sleep(w);
                continue;
            }
            self.process(next);
        }
        self.finish_adapter();
        self.meter(|m| m.flush());
        if self.unsupported_content_type {
            eprintln!("info: Uploading failed due to unsupported Content-Type header(s).\ninfo: Consider disabling Content-Type detection with:\ninfo:\ninfo:   $ git config lfs.contenttype false");
        }
    }

    pub fn take_errors(&mut self) -> Vec<Error> {
        std::mem::take(&mut self.errors)
    }
}

/// time.Duration's String() for a wait.
fn go_duration(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns == 0 {
        return "0s".into();
    }
    if ns < 1000 {
        return format!("{ns}ns");
    }
    if ns < 1_000_000 {
        return format!("{}µs", trim_float(ns as f64 / 1e3));
    }
    if ns < 1_000_000_000 {
        return format!("{}ms", trim_float(ns as f64 / 1e6));
    }
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        return format!("{}s", trim_float(secs));
    }
    let m = (secs / 60.0).floor();
    let s = secs - m * 60.0;
    if m < 60.0 {
        return format!("{}m{}s", m as u64, trim_float(s));
    }
    let h = (m / 60.0).floor();
    format!("{}h{}m{}s", h as u64, (m - h * 60.0) as u64, trim_float(s))
}

fn trim_float(x: f64) -> String {
    let s = format!("{x:.9}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

/// errors.Join's text: the messages one per line.
pub fn join_errors(errs: &[Error]) -> Error {
    Error::new(errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n"))
}

// What the filters use.

fn smudge_remote_ref() -> Ref {
    crate::gitcmd::default_remote_ref(&cfg().push_remote(), &cfg().current_ref())
}

fn download_with(remote: &str, ptr: &Pointer, name: &str, media: &str, cb: Option<ProgressCb>) -> Result<()> {
    let base = std::path::Path::new(name).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut q = TransferQueue::new(
        Direction::Download,
        Manifest::get("download", remote),
        remote,
        Options { remote_ref: Some(smudge_remote_ref()), batch_size: cfg().transfer_batch_size(), cb, ..Default::default() },
    );
    q.add(&base, media, &ptr.oid, ptr.size, false);
    q.wait();
    let errs = q.take_errors();
    if !errs.is_empty() {
        return Err(join_errors(&errs).wrap(format!("Error downloading {} ({})", name, ptr.oid)));
    }
    Ok(())
}

pub fn download_one(ptr: &Pointer, name: &str, media: &str, remote: &str, cb: Option<ProgressCb>) -> Result<()> {
    download_with(remote, ptr, name, media, cb)
}

/// downloadFileFallBack: each remote in turn; the first that has it becomes the remote.
pub fn download_fallback(ptr: &Pointer, name: &str, media: &str) -> Result<()> {
    let remotes = cfg().remotes();
    for (i, r) in remotes.iter().enumerate() {
        match download_with(r, ptr, name, media, None) {
            Ok(()) => {
                cfg().set_remote(r);
                return Ok(());
            }
            Err(e) => {
                if i + 1 >= remotes.len() {
                    return Err(e);
                }
                crate::trace!("git: download: remote failed {} {}", r, e);
            }
        }
    }
    Err(Error::new("No known remotes").wrap(format!("Error downloading {} ({})", name, ptr.oid)))
}

/// The delayed smudges of the filter process: one queue for all of them.
pub fn download_many(items: &[(String, Pointer)], remote: &str) -> Vec<Result<()>> {
    let fs = cfg().filesystem();
    let mut q = TransferQueue::new(
        Direction::Download,
        Manifest::get("download", remote),
        remote,
        Options { remote_ref: Some(smudge_remote_ref()), batch_size: cfg().transfer_batch_size(), ..Default::default() },
    );
    for (name, p) in items {
        let media = fs.object_path(&p.oid).unwrap_or_default();
        q.add(name, &media, &p.oid, p.size, false);
    }
    q.wait();
    let errs = q.take_errors();
    items
        .iter()
        .map(|(name, p)| {
            if fs.object_exists(&p.oid, p.size) {
                Ok(())
            } else if errs.is_empty() {
                Err(Error::new(format!("Error downloading {} ({})", name, p.oid)))
            } else {
                Err(join_errors(&errs).wrap(format!("Error downloading {} ({})", name, p.oid)))
            }
        })
        .collect()
}

pub fn first_remote_for_treeish(t: &str) -> String {
    crate::gitcmd::first_remote_for_treeish(t)
}

#[cfg(test)]
mod tests {
    #[test]
    fn zstd_cut_short() {
        // A frame, then a frame cut off after its header (as lfstest-gitserver sends it).
        let hex = "28b52ffd0400a9000073746f726167652d646f776e6c6f61642d656e636fc1e0fb0e28b52ffd040028b52ffd046801000099e9d851";
        let data: Vec<u8> = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap()).collect();
        let (out, err) = super::zstd_decode(&data);
        assert_eq!(out, b"storage-download-enco");
        assert!(err.is_some());
    }
}
