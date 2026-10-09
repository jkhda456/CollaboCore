//! Progress output (tasklog): tasks logged one after the other on stderr, their updates
//! throttled and shown only when stdout is a terminal (or progress is forced), each task's
//! last update always printed with ", done.".

use std::io::Write;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Update {
    pub s: String,
    pub at: Instant,
    pub force: bool,
}

pub struct Task {
    rx: Receiver<Update>,
    throttled: bool,
    /// Signalled when the task's last line is out (SimpleTask.Complete waits for it).
    done: Option<Sender<()>>,
}

pub struct Logger {
    queue: Mutex<Option<Sender<Task>>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

fn stdout_tty() -> bool {
    crate::tools::is_tty(1)
}

fn width() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
        ws.ws_col as usize
    } else {
        80
    }
}

fn log_task(t: Task, force_progress: bool, out: &mut dyn Write) {
    let throttle = Duration::from_millis(200);
    let mut last: Option<Instant> = None;
    let mut update: Option<Update> = None;
    for u in t.rx.iter() {
        if stdout_tty() || force_progress {
            let show = !t.throttled || u.force || last.is_none_or(|l| u.at > l + throttle);
            if show {
                let pad = width().saturating_sub(u.s.len());
                let _ = write!(out, "{}{}\r", u.s, " ".repeat(pad));
                let _ = out.flush();
                last = Some(u.at);
            }
        }
        update = Some(u);
    }
    if let Some(u) = update {
        let _ = write!(out, "{}, done.\n", u.s);
        let _ = out.flush();
    }
    if let Some(d) = t.done {
        let _ = d.send(());
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Sink {
    Stdout,
    Stderr,
    Discard,
}

impl Logger {
    pub fn new(sink: Sink, force_progress: bool) -> Arc<Logger> {
        let (tx, rx) = channel::<Task>();
        let worker = std::thread::spawn(move || {
            let mut out: Box<dyn Write> = match sink {
                Sink::Stdout => Box::new(std::io::stdout()),
                Sink::Stderr => Box::new(std::io::stderr()),
                Sink::Discard => Box::new(std::io::sink()),
            };
            for t in rx.iter() {
                log_task(t, force_progress, &mut *out);
            }
        });
        Arc::new(Logger { queue: Mutex::new(Some(tx)), worker: Mutex::new(Some(worker)) })
    }

    fn enqueue(&self, t: Task) {
        if let Some(q) = self.queue.lock().unwrap().as_ref() {
            let _ = q.send(t);
        }
    }

    /// Waits for every task to finish.
    pub fn close(&self) {
        self.queue.lock().unwrap().take();
        if let Some(w) = self.worker.lock().unwrap().take() {
            let _ = w.join();
        }
    }

    pub fn percentage(&self, msg: &str, total: u64) -> PercentageTask {
        let (tx, rx) = channel();
        let p = PercentageTask { msg: msg.to_string(), total, n: Mutex::new(0), tx: Mutex::new(Some(tx)) };
        p.count(0);
        self.enqueue(Task { rx, throttled: true, done: None });
        p
    }

    pub fn list(&self, msg: &str) -> ListTask {
        let (tx, rx) = channel();
        self.enqueue(Task { rx, throttled: false, done: None });
        ListTask { msg: msg.to_string(), tx: Some(tx) }
    }

    pub fn waiter(&self, msg: &str) -> WaitingTask {
        let (tx, rx) = channel();
        let _ = tx.send(Update { s: format!("{msg}: ..."), at: Instant::now(), force: false });
        self.enqueue(Task { rx, throttled: true, done: None });
        WaitingTask { tx: Some(tx) }
    }

    pub fn simple(&self) -> SimpleTask {
        let (tx, rx) = channel();
        let (dtx, drx) = channel();
        self.enqueue(Task { rx, throttled: false, done: Some(dtx) });
        SimpleTask { tx: Some(tx), done: Some(drx) }
    }

    /// A task fed by its own updates channel (the transfer meter).
    pub fn enqueue_channel(&self, rx: Receiver<Update>, throttled: bool) {
        self.enqueue(Task { rx, throttled, done: None });
    }
}

pub struct PercentageTask {
    msg: String,
    total: u64,
    n: Mutex<u64>,
    tx: Mutex<Option<Sender<Update>>>,
}

impl PercentageTask {
    pub fn count(&self, k: u64) -> u64 {
        let mut n = self.n.lock().unwrap();
        *n += k;
        let new = *n;
        let pct = if self.total == 0 { 100.0 } else { 100.0 * new as f64 / self.total as f64 };
        let mut tx = self.tx.lock().unwrap();
        if let Some(t) = tx.as_ref() {
            let _ = t.send(Update { s: format!("{}: {:3.0}% ({}/{})", self.msg, pct.floor(), new, self.total), at: Instant::now(), force: false });
        }
        if new >= self.total {
            tx.take();
        }
        new
    }
    pub fn entry(&self, s: &str) {
        if let Some(t) = self.tx.lock().unwrap().as_ref() {
            let _ = t.send(Update { s: format!("{s}\n"), at: Instant::now(), force: true });
        }
    }
    pub fn complete(&self) {
        *self.n.lock().unwrap() = self.total;
        self.tx.lock().unwrap().take();
    }
}

pub struct ListTask {
    msg: String,
    tx: Option<Sender<Update>>,
}

impl ListTask {
    pub fn entry(&self, s: &str) {
        if let Some(t) = &self.tx {
            let _ = t.send(Update { s: format!("{s}\n"), at: Instant::now(), force: false });
        }
    }
    pub fn complete(&mut self) {
        if let Some(t) = self.tx.take() {
            let _ = t.send(Update { s: format!("{}: ...", self.msg), at: Instant::now(), force: false });
        }
    }
}

pub struct WaitingTask {
    tx: Option<Sender<Update>>,
}

impl WaitingTask {
    pub fn complete(&mut self) {
        self.tx.take();
    }
}

pub struct SimpleTask {
    tx: Option<Sender<Update>>,
    done: Option<Receiver<()>>,
}

impl SimpleTask {
    pub fn log(&self, s: &str) {
        if let Some(t) = &self.tx {
            let _ = t.send(Update { s: s.to_string(), at: Instant::now(), force: false });
        }
    }
    pub fn complete(&mut self) {
        self.tx.take();
        if let Some(d) = self.done.take() {
            let _ = d.recv();
        }
    }
}
