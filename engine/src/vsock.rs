//! virtio-vsock (device id 19): stream connections between the host and the guest, addressed
//! by port. A port of the host JavaScript `virtio/vsock.ts`, with blocking streams instead of
//! promises: the guest's agent, the HTTP request API and the host-function bridge all speak
//! over one of these.
//!
//! Threading: the device itself is only touched on the machine's main thread (virtio work
//! happens there). Streams are used from other threads, so everything a stream needs lives
//! behind its own lock and outgoing packets go through the outbox, which wakes the machine.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use wasmtime::SharedMemory;

use crate::machine::Waker;
use crate::virtio::{write_bytes, Device, Queue};

const HOST_CID: u64 = 2;
const HEADER_SIZE: usize = 44;
const BUF_ALLOC: u32 = 256 * 1024;
const MAX_PAYLOAD: usize = 2048;
const TYPE_STREAM: u16 = 1;

const OP_REQUEST: u16 = 1;
const OP_RESPONSE: u16 = 2;
const OP_RST: u16 = 3;
const OP_SHUTDOWN: u16 = 4;
const OP_RW: u16 = 5;
const OP_CREDIT_UPDATE: u16 = 6;
const OP_CREDIT_REQUEST: u16 = 7;

const SHUTDOWN_BOTH: u32 = 1 | 2;

#[derive(Clone, Copy, Debug)]
struct Header {
    src_port: u32,
    dst_port: u32,
    len: u32,
    op: u16,
    buf_alloc: u32,
    fwd_cnt: u32,
}

impl Header {
    fn parse(bytes: &[u8]) -> Result<Header> {
        if bytes.len() < HEADER_SIZE {
            bail!("short vsock header: {} bytes", bytes.len());
        }
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        Ok(Header {
            src_port: u32_at(16),
            dst_port: u32_at(20),
            len: u32_at(24),
            op: u16::from_le_bytes(bytes[30..32].try_into().unwrap()),
            buf_alloc: u32_at(36),
            fwd_cnt: u32_at(40),
        })
    }
}

/// Everything a packet needs beyond its payload, so a stream can build one on any thread.
#[derive(Clone, Copy)]
struct Address {
    guest_cid: u64,
    local_port: u32,
    peer_port: u32,
}

fn packet(address: Address, op: u16, flags: u32, payload: &[u8], fwd_cnt: u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_SIZE + payload.len());
    bytes.extend_from_slice(&HOST_CID.to_le_bytes());
    bytes.extend_from_slice(&address.guest_cid.to_le_bytes());
    bytes.extend_from_slice(&address.local_port.to_le_bytes());
    bytes.extend_from_slice(&address.peer_port.to_le_bytes());
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&TYPE_STREAM.to_le_bytes());
    bytes.extend_from_slice(&op.to_le_bytes());
    bytes.extend_from_slice(&flags.to_le_bytes());
    bytes.extend_from_slice(&BUF_ALLOC.to_le_bytes());
    bytes.extend_from_slice(&fwd_cnt.to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

/// Packets waiting for the guest to offer a receive buffer. Any thread may add to it; the
/// machine's main thread drains it.
#[derive(Default)]
struct Outbox {
    packets: Mutex<VecDeque<Vec<u8>>>,
    waker: Mutex<Waker>,
}

impl Outbox {
    fn push(&self, packet: Vec<u8>) {
        self.packets.lock().unwrap().push_back(packet);
        self.waker.lock().unwrap().wake();
    }
}

/// The error a read returns when its deadline passes. The connection stays usable.
#[derive(Debug)]
pub struct ReadTimeout;

impl std::fmt::Display for ReadTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the guest did not answer in time")
    }
}

impl std::error::Error for ReadTimeout {}

#[derive(Default)]
struct ConnectionState {
    incoming: VecDeque<u8>,
    /// Set once the guest answered our REQUEST, or immediately for a connection it opened.
    connected: bool,
    /// The peer will send nothing more, and we may not write.
    closed: bool,
    /// Why the connection ended, when it was not an orderly close.
    error: Option<String>,
    bytes_read: u32,
    credited: u32,
    bytes_written: u32,
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    /// When set, a read that waits past this gives up (see `ReadTimeout`).
    deadline: Option<Instant>,
}

struct Connection {
    address: Address,
    outbox: Arc<Outbox>,
    state: Mutex<ConnectionState>,
    signal: Condvar,
}

impl Connection {
    fn new(address: Address, outbox: Arc<Outbox>, connected: bool) -> Arc<Connection> {
        Arc::new(Connection {
            address,
            outbox,
            state: Mutex::new(ConnectionState { connected, peer_buf_alloc: BUF_ALLOC, ..Default::default() }),
            signal: Condvar::new(),
        })
    }

    fn send(&self, op: u16, flags: u32, payload: &[u8], fwd_cnt: u32) {
        self.outbox.push(packet(self.address, op, flags, payload, fwd_cnt));
    }

    /// The peer is gone: wake everyone waiting on this connection.
    fn close_from_peer(&self, error: Option<String>) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        if state.error.is_none() {
            state.error = error;
        }
        self.signal.notify_all();
    }

    fn update_credit(&self, buf_alloc: u32, fwd_cnt: u32) {
        let mut state = self.state.lock().unwrap();
        state.peer_buf_alloc = buf_alloc;
        state.peer_fwd_cnt = fwd_cnt;
        self.signal.notify_all();
    }

    fn enqueue(&self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        state.incoming.extend(payload);
        self.signal.notify_all();
    }
}

/// One end of a vsock connection, used from any thread. Dropping it closes the connection.
pub struct VsockStream {
    connection: Arc<Connection>,
}

impl VsockStream {
    /// Makes later reads give up at `deadline` instead of waiting for the guest forever.
    pub fn set_deadline(&self, deadline: Option<Instant>) {
        self.connection.state.lock().unwrap().deadline = deadline;
    }

    /// Reads what has arrived, blocking until there is something or the peer closes. An empty
    /// result means the peer closed.
    pub fn read(&self, buffer: &mut [u8]) -> Result<usize> {
        let mut state = self.connection.state.lock().unwrap();
        loop {
            if !state.incoming.is_empty() {
                let take = state.incoming.len().min(buffer.len());
                for slot in buffer.iter_mut().take(take) {
                    *slot = state.incoming.pop_front().unwrap();
                }
                state.bytes_read = state.bytes_read.wrapping_add(take as u32);
                // Tell the guest it may send more once we have consumed a quarter of its window.
                let credit = state.bytes_read.wrapping_sub(state.credited) >= BUF_ALLOC / 4;
                if credit {
                    state.credited = state.bytes_read;
                }
                let fwd_cnt = state.bytes_read;
                drop(state);
                if credit {
                    self.connection.send(OP_CREDIT_UPDATE, 0, &[], fwd_cnt);
                }
                return Ok(take);
            }
            if state.closed {
                return match state.error.clone() {
                    Some(error) => bail!(error),
                    None => Ok(0),
                };
            }
            state = match state.deadline {
                None => self.connection.signal.wait(state).unwrap(),
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(anyhow::Error::new(ReadTimeout));
                    }
                    self.connection.signal.wait_timeout(state, left).unwrap().0
                }
            };
        }
    }

    /// Reads exactly `length` bytes, or fewer if the peer closes first.
    pub fn read_exact(&self, length: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(length);
        let mut chunk = vec![0u8; length.min(64 * 1024)];
        while out.len() < length {
            let want = (length - out.len()).min(chunk.len());
            let read = self.read(&mut chunk[..want])?;
            if read == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..read]);
        }
        Ok(out)
    }

    /// Writes everything, waiting for credit when the guest's receive window is full.
    pub fn write_all(&self, data: &[u8]) -> Result<()> {
        let mut offset = 0;
        while offset < data.len() {
            let (take, fwd_cnt) = {
                let mut state = self.connection.state.lock().unwrap();
                loop {
                    if state.closed {
                        bail!(state.error.clone().unwrap_or_else(|| "vsock connection is closed".into()));
                    }
                    let used = state.bytes_written.wrapping_sub(state.peer_fwd_cnt);
                    let available = state.peer_buf_alloc.saturating_sub(used) as usize;
                    if available > 0 {
                        let take = available.min(MAX_PAYLOAD).min(data.len() - offset);
                        state.bytes_written = state.bytes_written.wrapping_add(take as u32);
                        break (take, state.bytes_read);
                    }
                    state = self.connection.signal.wait(state).unwrap();
                }
            };
            self.connection.send(OP_RW, 0, &data[offset..offset + take], fwd_cnt);
            offset += take;
        }
        Ok(())
    }

    /// Closes this end. The guest sees end-of-file.
    pub fn close(&self) {
        let (already, fwd_cnt) = {
            let mut state = self.connection.state.lock().unwrap();
            let already = state.closed;
            state.closed = true;
            self.connection.signal.notify_all();
            (already, state.bytes_read)
        };
        if !already {
            self.connection.send(OP_SHUTDOWN, SHUTDOWN_BOTH, &[], fwd_cnt);
        }
    }
}

impl Drop for VsockStream {
    fn drop(&mut self) {
        self.close();
    }
}

/// How a connection is found when a packet arrives: the guest answers a connection we opened
/// on its local port alone, and addresses one it opened by both ports.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Local(u32),
    Pair(u32, u32),
}

type Listener = Arc<dyn Fn(VsockStream) + Send + Sync>;

#[derive(Default)]
struct Registry {
    connections: HashMap<Key, Arc<Connection>>,
    listeners: HashMap<u32, Listener>,
    next_port: u32,
    closed: bool,
}

/// Opens connections to the guest and accepts the ones it opens. Cloneable and usable from any
/// thread; the device does the work on the machine's main thread.
#[derive(Clone)]
pub struct Vsock {
    guest_cid: u64,
    outbox: Arc<Outbox>,
    registry: Arc<Mutex<Registry>>,
}

impl Vsock {
    pub fn new(guest_cid: u64) -> Vsock {
        Vsock {
            guest_cid,
            outbox: Arc::new(Outbox::default()),
            registry: Arc::new(Mutex::new(Registry { next_port: 49152, ..Default::default() })),
        }
    }

    /// The device to give the machine. One per `Vsock`.
    pub fn device(&self) -> Box<dyn Device> {
        Box::new(VsockDevice { vsock: self.clone() })
    }

    fn address(&self, local_port: u32, peer_port: u32) -> Address {
        Address { guest_cid: self.guest_cid, local_port, peer_port }
    }

    /// Connects to a listener inside the guest, waiting for it to answer.
    pub fn connect(&self, port: u32, timeout: Duration) -> Result<VsockStream> {
        let connection = {
            let mut registry = self.registry.lock().unwrap();
            if registry.closed {
                bail!("the vsock device is closed");
            }
            let mut local_port = None;
            for _ in 0..16384 {
                let port = registry.next_port;
                registry.next_port = if port == 65535 { 49152 } else { port + 1 };
                if !registry.connections.contains_key(&Key::Local(port)) {
                    local_port = Some(port);
                    break;
                }
            }
            let local_port = local_port.ok_or_else(|| anyhow::anyhow!("no local vsock ports available"))?;
            let connection = Connection::new(self.address(local_port, port), self.outbox.clone(), false);
            registry.connections.insert(Key::Local(local_port), connection.clone());
            connection
        };

        connection.send(OP_REQUEST, 0, &[], 0);
        let deadline = Instant::now() + timeout;
        let mut state = connection.state.lock().unwrap();
        while !state.connected && !state.closed {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(state);
                self.forget(Key::Local(connection.address.local_port));
                connection.send(OP_RST, 0, &[], 0);
                bail!("timed out connecting to guest vsock port {port}");
            }
            state = connection.signal.wait_timeout(state, left).unwrap().0;
        }
        if !state.connected {
            let reason = state.error.clone().unwrap_or_else(|| "the guest refused the connection".into());
            bail!("vsock port {port}: {reason}");
        }
        drop(state);
        Ok(VsockStream { connection })
    }

    /// Accepts the connections the guest opens to `port` on the host. The callback runs on the
    /// machine's main thread, so it must not block: hand the stream to a thread of its own.
    pub fn listen(&self, port: u32, on_connection: impl Fn(VsockStream) + Send + Sync + 'static) -> Result<()> {
        let mut registry = self.registry.lock().unwrap();
        if registry.listeners.contains_key(&port) {
            bail!("vsock port {port} already has a listener");
        }
        registry.listeners.insert(port, Arc::new(on_connection));
        Ok(())
    }

    fn forget(&self, key: Key) -> Option<Arc<Connection>> {
        self.registry.lock().unwrap().connections.remove(&key)
    }

    fn lookup(&self, header: &Header) -> Option<Arc<Connection>> {
        let registry = self.registry.lock().unwrap();
        registry
            .connections
            .get(&Key::Pair(header.dst_port, header.src_port))
            .or_else(|| registry.connections.get(&Key::Local(header.dst_port)))
            .cloned()
    }

    /// The guest opened a connection to one of our ports: accept it, or refuse it at once.
    fn accept(&self, header: &Header) {
        let key = Key::Pair(header.dst_port, header.src_port);
        let listener = {
            let mut registry = self.registry.lock().unwrap();
            let listener = registry.listeners.get(&header.dst_port).cloned();
            match listener {
                Some(listener) if !registry.closed && !registry.connections.contains_key(&key) => {
                    let connection =
                        Connection::new(self.address(header.dst_port, header.src_port), self.outbox.clone(), true);
                    connection.update_credit(header.buf_alloc, header.fwd_cnt);
                    registry.connections.insert(key, connection.clone());
                    Some((listener, connection))
                }
                _ => None,
            }
        };
        match listener {
            Some((listener, connection)) => {
                connection.send(OP_RESPONSE, 0, &[], 0);
                listener(VsockStream { connection });
            }
            None => {
                // Nothing listens there: refuse now instead of leaving the guest waiting.
                let address = self.address(header.dst_port, header.src_port);
                self.outbox.push(packet(address, OP_RST, 0, &[], 0));
            }
        }
    }

    fn handle(&self, header: &Header, payload: &[u8]) {
        if header.op == OP_REQUEST {
            self.accept(header);
            return;
        }
        let Some(connection) = self.lookup(header) else { return };
        connection.update_credit(header.buf_alloc, header.fwd_cnt);
        match header.op {
            OP_RESPONSE => {
                let mut state = connection.state.lock().unwrap();
                state.connected = true;
                connection.signal.notify_all();
            }
            OP_RW => connection.enqueue(payload),
            OP_CREDIT_UPDATE => {}
            OP_CREDIT_REQUEST => {
                let fwd_cnt = connection.state.lock().unwrap().bytes_read;
                connection.send(OP_CREDIT_UPDATE, 0, &[], fwd_cnt);
            }
            OP_SHUTDOWN | OP_RST => {
                let connected = connection.state.lock().unwrap().connected;
                if header.op == OP_SHUTDOWN {
                    connection.send(OP_RST, 0, &[], 0);
                }
                connection.close_from_peer(match connected {
                    true => None,
                    false => Some("the guest reset the connection".into()),
                });
                self.forget(Key::Pair(header.dst_port, header.src_port));
                self.forget(Key::Local(header.dst_port));
            }
            op => eprintln!("collabo-core: unknown vsock op {op}"),
        }
    }

    /// Every connection ends when the device is reset or the machine stops.
    fn reset(&self, reason: &str) {
        let mut registry = self.registry.lock().unwrap();
        for connection in registry.connections.values() {
            connection.close_from_peer(Some(reason.to_string()));
        }
        registry.connections.clear();
        self.outbox.packets.lock().unwrap().clear();
    }
}

struct VsockDevice {
    vsock: Vsock,
}

impl Device for VsockDevice {
    fn device_id(&self) -> u32 {
        19
    }

    fn config(&self) -> Vec<u8> {
        self.vsock.guest_cid.to_le_bytes().to_vec()
    }

    fn attach_waker(&mut self, waker: Waker) {
        *self.vsock.outbox.waker.lock().unwrap() = waker;
    }

    fn notify(&mut self, queue_index: u16, queue: &mut Queue, memory: &SharedMemory) -> Result<Vec<u32>> {
        // Queue 0 (rx) is the guest offering buffers, drained by `poll`; queue 2 carries device
        // events, which only matter for host transport reset.
        if queue_index != 1 {
            return Ok(Vec::new());
        }
        let mut irqs = Vec::new();
        while let Some(chain) = queue.pop()? {
            let mut bytes = Vec::new();
            for buffer in &chain.buffers {
                if buffer.writable {
                    bail!("vsock tx descriptor must be readable");
                }
                match crate::machine::guest_bytes(memory, buffer.address as u32, buffer.length) {
                    Some(part) => bytes.extend_from_slice(&part),
                    None => bail!("vsock tx buffer outside guest memory"),
                }
            }
            let header = Header::parse(&bytes)?;
            let end = (HEADER_SIZE + header.len as usize).min(bytes.len());
            self.vsock.handle(&header, &bytes[HEADER_SIZE.min(end)..end]);
            irqs.push(queue.release(chain, 0)?);
        }
        Ok(irqs)
    }

    fn poll(&mut self, queues: &mut [Option<Queue>], memory: &SharedMemory) -> Result<Vec<u32>> {
        let Some(Some(queue)) = queues.first_mut() else { return Ok(Vec::new()) };
        let mut irqs = Vec::new();
        loop {
            let packet = {
                let mut packets = self.vsock.outbox.packets.lock().unwrap();
                match packets.front() {
                    Some(_) => packets.pop_front().unwrap(),
                    None => break,
                }
            };
            let Some(chain) = queue.pop()? else {
                // No buffer to put it in: keep it for the next kick.
                self.vsock.outbox.packets.lock().unwrap().push_front(packet);
                break;
            };
            let mut written = 0usize;
            for buffer in &chain.buffers {
                if !buffer.writable || written >= packet.len() {
                    continue;
                }
                let take = (buffer.length as usize).min(packet.len() - written);
                let done = write_bytes(memory, buffer.address, &packet[written..written + take]);
                written += done;
                if done < take {
                    break;
                }
            }
            if written < packet.len() {
                bail!("vsock rx buffer too small: {written} of {} bytes", packet.len());
            }
            irqs.push(queue.release(chain, written as u32)?);
        }
        Ok(irqs)
    }

    fn reset(&mut self) {
        self.vsock.reset("the vsock device was reset");
    }
}
