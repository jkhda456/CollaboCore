//! virtio-net (device id 1) plus the small TCP/IP stack behind it, so sockets inside the guest
//! reach the network through the host — the packet-level counterpart of the request API in
//! `http.rs`, and the Rust side of what `@lowland/guest`'s JavaScript stack did.
//!
//! The guest sees one Ethernet link with a gateway that is this process:
//!
//!   guest 192.0.2.2  ──  gateway 192.0.2.1 (this host; also its own 127.0.0.1)
//!
//! What the stack answers: ARP for the gateway, ICMP echo, DNS on the gateway (resolved by the
//! host, filtered by the same policy), and TCP, where every connection is terminated here and
//! proxied to a real socket. UDP other than DNS is not carried.
//!
//! Nothing is ever dropped on the way to the guest: frames wait in a queue until the guest
//! offers a buffer, so the stack needs no retransmission of its own.
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;
use wasmtime::SharedMemory;

use crate::http::{Asker, Policy};
use crate::intercept::Interceptor;
use crate::machine::Waker;
use crate::virtio::{write_bytes, Device, Queue};

/// `struct virtio_net_hdr_v1`, which modern virtio-net always uses.
const NET_HEADER: usize = 12;
const FEATURE_MAC: u64 = 1 << 5;

pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
pub const GUEST: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 2);
const GATEWAY_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x00, 0x00, 0x01];
const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const PROTO_ICMP: u8 = 1;
const PROTO_TCP: u8 = 6;
const PROTO_UDP: u8 = 17;

const FIN: u8 = 1;
const SYN: u8 = 1 << 1;
const RST: u8 = 1 << 2;
const PSH: u8 = 1 << 3;
const ACK: u8 = 1 << 4;

/// What the guest may receive at once, and the most we send before it acknowledges.
const WINDOW: u32 = 64 * 1024;
const MAX_SEGMENT: usize = 1400;

/// What the host tells the app about the guest's packet-level activity.
#[derive(Debug, Clone)]
pub enum Event {
    Dns { host: String, addresses: Vec<String>, blocked: bool, reason: Option<String> },
    Connect { id: u64, ip: String, port: u16, phase: &'static str, blocked: bool, reason: Option<String> },
}

pub type Observer = Arc<dyn Fn(Event) + Send + Sync>;

fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum = 0u32;
    let mut odd = None;
    for part in parts {
        let mut bytes = *part;
        if let Some(first) = odd.take() {
            if let Some((second, rest)) = bytes.split_first() {
                sum += u16::from_be_bytes([first, *second]) as u32;
                bytes = rest;
            } else {
                odd = Some(first);
            }
        }
        let mut chunks = bytes.chunks_exact(2);
        for chunk in &mut chunks {
            sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
        }
        if let Some(last) = chunks.remainder().first() {
            odd = Some(*last);
        }
    }
    if let Some(last) = odd {
        sum += u16::from_be_bytes([last, 0]) as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// A frame on its way to the guest.
type Frame = Vec<u8>;

fn ethernet(payload_type: u16, payload: &[u8]) -> Frame {
    let mut frame = Vec::with_capacity(14 + payload.len());
    frame.extend_from_slice(&GUEST_MAC);
    frame.extend_from_slice(&GATEWAY_MAC);
    frame.extend_from_slice(&payload_type.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// An IPv4 packet from the gateway to the guest.
fn ipv4(protocol: u8, source: Ipv4Addr, payload: &[u8]) -> Frame {
    let total = 20 + payload.len();
    let mut header = Vec::with_capacity(20);
    header.push(0x45); // version 4, 5 words of header
    header.push(0); // dscp
    header.extend_from_slice(&(total as u16).to_be_bytes());
    header.extend_from_slice(&0u16.to_be_bytes()); // identification
    header.extend_from_slice(&0x4000u16.to_be_bytes()); // don't fragment
    header.push(64); // ttl
    header.push(protocol);
    header.extend_from_slice(&0u16.to_be_bytes()); // checksum, filled in below
    header.extend_from_slice(&source.octets());
    header.extend_from_slice(&GUEST.octets());
    let sum = checksum(&[&header]);
    header[10..12].copy_from_slice(&sum.to_be_bytes());
    header.extend_from_slice(payload);
    ethernet(ETHERTYPE_IPV4, &header)
}

/// The checksum over the TCP/UDP pseudo-header and the segment.
fn transport_checksum(protocol: u8, source: Ipv4Addr, destination: Ipv4Addr, segment: &[u8]) -> u16 {
    let mut pseudo = Vec::with_capacity(12);
    pseudo.extend_from_slice(&source.octets());
    pseudo.extend_from_slice(&destination.octets());
    pseudo.push(0);
    pseudo.push(protocol);
    pseudo.extend_from_slice(&(segment.len() as u16).to_be_bytes());
    checksum(&[&pseudo, segment])
}

/// One end of a TCP connection the guest opened, with the host socket behind it.
struct Connection {
    id: u64,
    /// The address the guest asked for, as it sees it.
    guest_target: Ipv4Addr,
    guest_port: u16,
    port: u16,
    /// Our sequence numbers.
    send_next: u32,
    send_unacked: u32,
    /// The next byte we expect from the guest.
    receive_next: u32,
    /// What the guest says it can take.
    peer_window: u32,
    /// Data from the host socket, waiting for the guest's window.
    outgoing: VecDeque<u8>,
    /// To the host socket's writer thread.
    to_host: Option<Sender<Vec<u8>>>,
    socket: Option<TcpStream>,
    established: bool,
    /// The host closed its side and everything before this sequence number is sent.
    host_closed: bool,
    fin_sent: bool,
    guest_finished: bool,
    closed: bool,
}

impl Connection {
    /// The bytes the guest still has room for.
    fn sendable(&self) -> usize {
        let in_flight = self.send_next.wrapping_sub(self.send_unacked) as usize;
        (self.peer_window as usize).saturating_sub(in_flight).min(self.outgoing.len())
    }
}

#[derive(Default)]
struct Pending {
    frames: VecDeque<Frame>,
}

/// The stack: every connection, and the frames waiting for the guest.
pub struct Stack {
    policy: Arc<RwLock<Policy>>,
    observer: Option<Observer>,
    waker: Mutex<Waker>,
    pending: Mutex<Pending>,
    connections: Mutex<HashMap<(u16, Ipv4Addr, u16), Connection>>,
    /// Addresses DNS handed out, and the name that asked for them: a connection to an address
    /// is allowed when the name it came from is.
    resolved: Mutex<HashMap<Ipv4Addr, Vec<String>>>,
    next_id: Mutex<u64>,
    /// Asks the app about hosts neither list names, when the policy says `ask`.
    asker: Option<Asker>,
    /// Takes the TLS connections to hosts that have secrets, to add them (intercept.rs).
    interceptor: Option<Arc<Interceptor>>,
}

/// What to do with a connection the guest opens.
enum Decision {
    Allow,
    Refuse(String),
    /// Ask the app about this "host:port" first.
    Ask(String),
}

impl Stack {
    pub fn new(
        policy: Arc<RwLock<Policy>>,
        observer: Option<Observer>,
        asker: Option<Asker>,
        interceptor: Option<Arc<Interceptor>>,
    ) -> Arc<Stack> {
        Arc::new(Stack {
            policy,
            observer,
            waker: Mutex::new(Waker::default()),
            pending: Mutex::new(Pending::default()),
            connections: Mutex::new(HashMap::new()),
            resolved: Mutex::new(HashMap::new()),
            next_id: Mutex::new(1),
            asker,
            interceptor,
        })
    }

    /// The device to give the machine.
    pub fn device(self: &Arc<Stack>) -> Box<dyn Device> {
        Box::new(NetDevice { stack: self.clone() })
    }

    /// The kernel command line that tells the guest its address.
    pub fn kernel_arguments() -> Vec<String> {
        vec![format!("collabo.ip={GUEST}"), format!("collabo.gw={GATEWAY}")]
    }

    fn report(&self, event: Event) {
        if let Some(observer) = &self.observer {
            observer(event);
        }
    }

    fn send(&self, frame: Frame) {
        self.pending.lock().unwrap().frames.push_back(frame);
        self.waker.lock().unwrap().wake();
    }

    /// May the guest reach this address (as `address`, really `target`) on `port`?
    fn decide(&self, address: Ipv4Addr, target: Ipv4Addr, port: u16) -> Decision {
        let policy = self.policy.read().unwrap();
        // The gateway is this computer: `allowHostLoopback` alone governs it.
        if target.is_loopback() {
            return match policy.allow_loopback {
                true => Decision::Allow,
                false => Decision::Refuse("the host's own services (192.0.2.1 = its localhost) are not allowed".into()),
            };
        }
        let names = self.resolved.lock().unwrap().get(&address).cloned().unwrap_or_default();
        if names.iter().any(|name| policy.refuses_host(name).is_none()) {
            return Decision::Allow;
        }
        let ip = address.to_string();
        let Some(reason) = policy.refuses_host(&ip) else { return Decision::Allow };
        // A name (or, without one, the address) that no list mentions: the app decides.
        if policy.ask && self.asker.is_some() {
            let unlisted = match names.is_empty() {
                true => policy.unlisted(&ip).then_some(ip.as_str()),
                false => names.iter().find(|name| policy.unlisted(name)).map(String::as_str),
            };
            if let Some(host) = unlisted {
                return Decision::Ask(format!("{host}:{port}"));
            }
        }
        Decision::Refuse(match names.first() {
            Some(name) => policy.refuses_host(name).unwrap_or(reason),
            None => format!("{address} was not resolved from an allowed name"),
        })
    }

    // ---- what arrives from the guest ----------------------------------------------------

    fn receive_frame(self: &Arc<Stack>, frame: &[u8]) {
        if frame.len() < 14 {
            return;
        }
        match u16::from_be_bytes([frame[12], frame[13]]) {
            ETHERTYPE_ARP => self.receive_arp(&frame[14..]),
            ETHERTYPE_IPV4 => self.receive_ipv4(&frame[14..]),
            _ => {}
        }
    }

    /// Answers "who has 192.0.2.1" so the guest can send us anything at all.
    fn receive_arp(&self, packet: &[u8]) {
        if packet.len() < 28 || u16::from_be_bytes([packet[6], packet[7]]) != 1 {
            return; // not an ARP request
        }
        let target = Ipv4Addr::new(packet[24], packet[25], packet[26], packet[27]);
        if target != GATEWAY {
            return;
        }
        let sender_mac = &packet[8..14];
        let sender_ip = &packet[14..18];
        let mut reply = Vec::with_capacity(28);
        reply.extend_from_slice(&[0, 1]); // ethernet
        reply.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        reply.extend_from_slice(&[6, 4, 0, 2]); // lengths, reply
        reply.extend_from_slice(&GATEWAY_MAC);
        reply.extend_from_slice(&GATEWAY.octets());
        reply.extend_from_slice(sender_mac);
        reply.extend_from_slice(sender_ip);
        self.send(ethernet(ETHERTYPE_ARP, &reply));
    }

    fn receive_ipv4(self: &Arc<Stack>, packet: &[u8]) {
        if packet.len() < 20 || packet[0] >> 4 != 4 {
            return;
        }
        let header_length = (packet[0] & 0xf) as usize * 4;
        let total = u16::from_be_bytes([packet[2], packet[3]]) as usize;
        if packet.len() < header_length || total < header_length || packet.len() < total {
            return;
        }
        let protocol = packet[9];
        let destination = Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]);
        let payload = &packet[header_length..total];
        match protocol {
            PROTO_ICMP => self.receive_icmp(destination, payload),
            PROTO_UDP => self.receive_udp(destination, payload),
            PROTO_TCP => self.receive_tcp(destination, payload),
            _ => {}
        }
    }

    /// Answers a ping to the gateway, which is how the guest checks the link.
    fn receive_icmp(&self, destination: Ipv4Addr, payload: &[u8]) {
        if destination != GATEWAY || payload.len() < 8 || payload[0] != 8 {
            return;
        }
        let mut reply = payload.to_vec();
        reply[0] = 0; // echo reply
        reply[2..4].copy_from_slice(&[0, 0]);
        let sum = checksum(&[&reply]);
        reply[2..4].copy_from_slice(&sum.to_be_bytes());
        self.send(ipv4(PROTO_ICMP, destination, &reply));
    }

    // ---- DNS ----------------------------------------------------------------------------

    fn receive_udp(&self, destination: Ipv4Addr, payload: &[u8]) {
        if payload.len() < 8 {
            return;
        }
        let source_port = u16::from_be_bytes([payload[0], payload[1]]);
        let destination_port = u16::from_be_bytes([payload[2], payload[3]]);
        let length = u16::from_be_bytes([payload[4], payload[5]]) as usize;
        if destination_port != 53 || length < 8 || payload.len() < length {
            return;
        }
        let Some(answer) = self.answer_dns(&payload[8..length]) else { return };

        let mut datagram = Vec::with_capacity(8 + answer.len());
        datagram.extend_from_slice(&destination_port.to_be_bytes());
        datagram.extend_from_slice(&source_port.to_be_bytes());
        datagram.extend_from_slice(&((8 + answer.len()) as u16).to_be_bytes());
        datagram.extend_from_slice(&0u16.to_be_bytes());
        datagram.extend_from_slice(&answer);
        let sum = transport_checksum(PROTO_UDP, destination, GUEST, &datagram);
        datagram[6..8].copy_from_slice(&if sum == 0 { 0xffff } else { sum }.to_be_bytes());
        self.send(ipv4(PROTO_UDP, destination, &datagram));
    }

    /// Resolves one question with the host's resolver, under the same policy as everything else.
    fn answer_dns(&self, query: &[u8]) -> Option<Vec<u8>> {
        if query.len() < 12 {
            return None;
        }
        let id = u16::from_be_bytes([query[0], query[1]]);
        if u16::from_be_bytes([query[4], query[5]]) != 1 {
            return None; // exactly one question, as every resolver sends
        }
        let (name, at) = read_name(query, 12)?;
        if query.len() < at + 4 {
            return None;
        }
        let kind = u16::from_be_bytes([query[at], query[at + 1]]);
        let question = &query[12..at + 4];

        // 3 = NXDOMAIN, which is what a refused or unknown name looks like to the guest.
        let mut code = 3u8;
        let mut addresses: Vec<Ipv4Addr> = Vec::new();
        let name = name.trim_end_matches('.').to_lowercase();
        let refused = {
            let policy = self.policy.read().unwrap();
            // With `ask`, an unlisted name resolves: the question comes when the guest connects,
            // with the port, and the address alone lets nothing through.
            policy.refuses_host(&name).filter(|_| !(policy.ask && self.asker.is_some() && policy.unlisted(&name)))
        };
        match refused {
            Some(reason) => {
                self.report(Event::Dns { host: name.clone(), addresses: Vec::new(), blocked: true, reason: Some(reason) });
            }
            None => {
                // AAAA is answered with "no such record" so the guest falls back to IPv4.
                if kind == 1 {
                    match (name.as_str(), 0u16).to_socket_addrs() {
                        Ok(found) => {
                            addresses = found
                                .filter_map(|address| match address.ip() {
                                    IpAddr::V4(ip) => Some(ip),
                                    IpAddr::V6(_) => None,
                                })
                                .collect();
                            addresses.dedup();
                            if !addresses.is_empty() {
                                code = 0;
                                let mut resolved = self.resolved.lock().unwrap();
                                for address in &addresses {
                                    let names = resolved.entry(*address).or_default();
                                    if !names.contains(&name) {
                                        names.push(name.clone());
                                    }
                                }
                            }
                            self.report(Event::Dns {
                                host: name.clone(),
                                addresses: addresses.iter().map(|a| a.to_string()).collect(),
                                blocked: false,
                                reason: None,
                            });
                        }
                        Err(error) => self.report(Event::Dns {
                            host: name.clone(),
                            addresses: Vec::new(),
                            blocked: false,
                            reason: Some(error.to_string()),
                        }),
                    }
                } else {
                    code = 0; // the name exists, just not with this record type
                }
            }
        }

        let mut response = Vec::with_capacity(12 + question.len() + addresses.len() * 16);
        response.extend_from_slice(&id.to_be_bytes());
        response.extend_from_slice(&(0x8180u16 | code as u16).to_be_bytes()); // response, recursion available
        response.extend_from_slice(&1u16.to_be_bytes()); // questions
        response.extend_from_slice(&(addresses.len() as u16).to_be_bytes());
        response.extend_from_slice(&0u16.to_be_bytes()); // authority
        response.extend_from_slice(&0u16.to_be_bytes()); // additional
        response.extend_from_slice(question);
        for address in &addresses {
            response.extend_from_slice(&0xc00cu16.to_be_bytes()); // the name in the question
            response.extend_from_slice(&1u16.to_be_bytes()); // A
            response.extend_from_slice(&1u16.to_be_bytes()); // IN
            response.extend_from_slice(&60u32.to_be_bytes()); // ttl
            response.extend_from_slice(&4u16.to_be_bytes());
            response.extend_from_slice(&address.octets());
        }
        Some(response)
    }

    // ---- TCP ----------------------------------------------------------------------------

    fn receive_tcp(self: &Arc<Stack>, destination: Ipv4Addr, segment: &[u8]) {
        if segment.len() < 20 {
            return;
        }
        let source_port = u16::from_be_bytes([segment[0], segment[1]]);
        let destination_port = u16::from_be_bytes([segment[2], segment[3]]);
        let sequence = u32::from_be_bytes([segment[4], segment[5], segment[6], segment[7]]);
        let acknowledged = u32::from_be_bytes([segment[8], segment[9], segment[10], segment[11]]);
        let offset = (segment[12] >> 4) as usize * 4;
        let flags = segment[13];
        let window = u16::from_be_bytes([segment[14], segment[15]]) as u32;
        if segment.len() < offset {
            return;
        }
        let data = &segment[offset..];
        let key = (source_port, destination, destination_port);

        if flags & SYN != 0 && flags & ACK == 0 {
            self.open(key, sequence, window, destination, destination_port, source_port);
            return;
        }

        let mut connections = self.connections.lock().unwrap();
        let Some(connection) = connections.get_mut(&key) else {
            // Nothing here: tell the guest so it fails at once instead of waiting.
            if flags & RST == 0 {
                let reply = tcp_segment(destination, source_port, destination_port, acknowledged, 0, RST, 0, &[]);
                drop(connections);
                self.send(reply);
            }
            return;
        };
        connection.peer_window = window;

        if flags & RST != 0 {
            let id = connection.id;
            let (ip, port) = (connection.guest_target.to_string(), connection.port);
            connection.closed = true;
            close_socket(connection);
            connections.remove(&key);
            drop(connections);
            self.report(Event::Connect { id, ip, port, phase: "closed", blocked: false, reason: None });
            return;
        }

        if flags & ACK != 0 {
            let acked = acknowledged.wrapping_sub(connection.send_unacked);
            if acked > 0 && acked <= connection.send_next.wrapping_sub(connection.send_unacked) {
                connection.send_unacked = acknowledged;
            }
        }

        // Only data that continues where the last left off; anything else is re-acknowledged
        // and the guest sends it again.
        if !data.is_empty() && sequence == connection.receive_next {
            connection.receive_next = connection.receive_next.wrapping_add(data.len() as u32);
            if let Some(to_host) = &connection.to_host {
                let _ = to_host.send(data.to_vec());
            }
        }
        if flags & FIN != 0 && sequence.wrapping_add(data.len() as u32) == connection.receive_next {
            connection.receive_next = connection.receive_next.wrapping_add(1);
            connection.guest_finished = true;
            // The guest will send nothing more: let the host peer see the end of the stream.
            connection.to_host = None;
            if let Some(socket) = &connection.socket {
                let _ = socket.shutdown(Shutdown::Write);
            }
        }
        let acknowledge = !data.is_empty() || flags & FIN != 0;
        drop(connections);
        self.flush(Some(key), acknowledge);
    }

    /// A SYN: check the policy, then connect on the host in a thread of its own.
    fn open(self: &Arc<Stack>, key: (u16, Ipv4Addr, u16), sequence: u32, window: u32, destination: Ipv4Addr, port: u16, guest_port: u16) {
        let id = {
            let mut next = self.next_id.lock().unwrap();
            *next += 1;
            *next
        };
        // The gateway is this computer, so that is where the connection really goes.
        let target = if destination == GATEWAY { Ipv4Addr::LOCALHOST } else { destination };
        let asking = match self.decide(destination, target, port) {
            Decision::Allow => None,
            Decision::Refuse(reason) => return self.refuse(id, destination, port, guest_port, sequence, reason),
            Decision::Ask(what) => Some(what),
        };

        let initial = rand_sequence();
        {
            let mut connections = self.connections.lock().unwrap();
            if connections.contains_key(&key) {
                return; // a repeat of a SYN we are already working on
            }
            connections.insert(
                key,
                Connection {
                    id,
                    guest_target: destination,
                    guest_port,
                    port,
                    send_next: initial,
                    send_unacked: initial,
                    receive_next: sequence.wrapping_add(1),
                    peer_window: window.max(1),
                    outgoing: VecDeque::new(),
                    to_host: None,
                    socket: None,
                    established: false,
                    host_closed: false,
                    fin_sent: false,
                    guest_finished: false,
                    closed: false,
                },
            );
        }
        let address = SocketAddr::new(IpAddr::V4(target), port);
        let Some(what) = asking else {
            self.report(Event::Connect { id, ip: destination.to_string(), port, phase: "open", blocked: false, reason: None });
            return self.connect(key, address);
        };
        // The user may take a while; the guest keeps resending its SYN, which the connection
        // entry above absorbs, until the answer.
        let (stack, asker) = (self.clone(), self.asker.clone().expect("Decision::Ask needs an asker"));
        std::thread::spawn(move || match asker("net", &what) {
            true => {
                stack.report(Event::Connect { id, ip: destination.to_string(), port, phase: "open", blocked: false, reason: None });
                stack.connect(key, address);
            }
            false => {
                stack.connections.lock().unwrap().remove(&key);
                let reason = format!("blocked by the network policy: the app did not allow \"{what}\"");
                stack.refuse(id, destination, port, guest_port, sequence, reason);
            }
        });
    }

    /// Turns a connection away: the guest sees it reset, the app sees why.
    fn refuse(&self, id: u64, destination: Ipv4Addr, port: u16, guest_port: u16, sequence: u32, reason: String) {
        self.report(Event::Connect {
            id,
            ip: destination.to_string(),
            port,
            phase: "failed",
            blocked: true,
            reason: Some(reason),
        });
        self.send(tcp_segment(destination, guest_port, port, 0, sequence.wrapping_add(1), RST | ACK, 0, &[]));
    }

    /// The interceptor, when this connection goes to a host the app has secrets for.
    fn interceptor_for(&self, address: SocketAddr, guest_target: Ipv4Addr) -> Option<Arc<Interceptor>> {
        let interceptor = self.interceptor.as_ref()?;
        let names = match address.ip().is_loopback() {
            true => vec!["localhost".to_string()],
            false => self.resolved.lock().unwrap().get(&guest_target).cloned().unwrap_or_default(),
        };
        let policy = self.policy.read().unwrap();
        names.iter().any(|name| policy.has_secret_for(name)).then(|| interceptor.clone())
    }

    /// Connects on the host; the guest waits for the SYN-ACK this produces.
    fn connect(self: &Arc<Stack>, key: (u16, Ipv4Addr, u16), address: SocketAddr) {
        let owner = self.clone();
        let guest_target = key.1;
        std::thread::spawn(move || {
            let socket = match owner.interceptor_for(address, guest_target) {
                Some(interceptor) => interceptor.attach(address),
                None => TcpStream::connect_timeout(&address, Duration::from_secs(10)),
            };
            let mut connections = owner.connections.lock().unwrap();
            let Some(connection) = connections.get_mut(&key) else { return };
            let (id, ip, port) = (connection.id, connection.guest_target.to_string(), connection.port);
            match socket {
                Ok(socket) => {
                    let _ = socket.set_nodelay(true);
                    connection.socket = socket.try_clone().ok();
                    connection.established = true;
                    let (sender, receiver) = channel::<Vec<u8>>();
                    connection.to_host = Some(sender);
                    let reply = tcp_segment(
                        connection.guest_target,
                        connection.guest_port,
                        connection.port,
                        connection.send_next,
                        connection.receive_next,
                        SYN | ACK,
                        WINDOW,
                        &[],
                    );
                    connection.send_next = connection.send_next.wrapping_add(1);
                    connection.send_unacked = connection.send_next.wrapping_sub(1);
                    drop(connections);
                    owner.send(reply);

                    // One thread writes what the guest sends, another reads what comes back.
                    let writer = socket.try_clone();
                    std::thread::spawn(move || {
                        let Ok(mut socket) = writer else { return };
                        while let Ok(bytes) = receiver.recv() {
                            if socket.write_all(&bytes).is_err() {
                                break;
                            }
                        }
                        let _ = socket.shutdown(Shutdown::Write);
                    });
                    owner.read_from_host(key, socket);
                }
                Err(error) => {
                    connection.closed = true;
                    let reply = tcp_segment(
                        connection.guest_target,
                        connection.guest_port,
                        connection.port,
                        connection.send_next,
                        connection.receive_next,
                        RST | ACK,
                        0,
                        &[],
                    );
                    connections.remove(&key);
                    drop(connections);
                    owner.send(reply);
                    owner.report(Event::Connect {
                        id,
                        ip,
                        port,
                        phase: "failed",
                        blocked: false,
                        reason: Some(error.to_string()),
                    });
                }
            }
        });
    }

    /// Reads the host socket until it ends, handing what it gets to the guest.
    fn read_from_host(self: &Arc<Stack>, key: (u16, Ipv4Addr, u16), mut socket: TcpStream) {
        let stack = self.clone();
        std::thread::spawn(move || {
            let mut buffer = vec![0u8; 32 * 1024];
            loop {
                match socket.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let mut connections = stack.connections.lock().unwrap();
                        let Some(connection) = connections.get_mut(&key) else { break };
                        connection.outgoing.extend(&buffer[..read]);
                        drop(connections);
                        stack.flush(Some(key), false);
                    }
                }
            }
            {
                let mut connections = stack.connections.lock().unwrap();
                if let Some(connection) = connections.get_mut(&key) {
                    connection.host_closed = true;
                }
            }
            stack.flush(Some(key), false);
        });
    }

    /// Sends what it can of every connection's data, then a FIN when the host is done.
    fn flush(&self, only: Option<(u16, Ipv4Addr, u16)>, acknowledge: bool) {
        let mut frames: Vec<Frame> = Vec::new();
        let mut finished: Vec<((u16, Ipv4Addr, u16), u64, String, u16)> = Vec::new();
        {
            let mut connections = self.connections.lock().unwrap();
            let keys: Vec<_> = match only {
                Some(key) => vec![key],
                None => connections.keys().copied().collect(),
            };
            for key in keys {
                let Some(connection) = connections.get_mut(&key) else { continue };
                if !connection.established {
                    continue;
                }
                while connection.sendable() > 0 {
                    let take = connection.sendable().min(MAX_SEGMENT);
                    let data: Vec<u8> = connection.outgoing.drain(..take).collect();
                    frames.push(tcp_segment(
                        connection.guest_target,
                        connection.guest_port,
                        connection.port,
                        connection.send_next,
                        connection.receive_next,
                        ACK | PSH,
                        WINDOW,
                        &data,
                    ));
                    connection.send_next = connection.send_next.wrapping_add(data.len() as u32);
                }
                if connection.host_closed && connection.outgoing.is_empty() && !connection.fin_sent {
                    frames.push(tcp_segment(
                        connection.guest_target,
                        connection.guest_port,
                        connection.port,
                        connection.send_next,
                        connection.receive_next,
                        ACK | FIN,
                        WINDOW,
                        &[],
                    ));
                    connection.send_next = connection.send_next.wrapping_add(1);
                    connection.fin_sent = true;
                } else if acknowledge && frames.is_empty() {
                    // Nothing to carry: acknowledge what the guest sent.
                    frames.push(tcp_segment(
                        connection.guest_target,
                        connection.guest_port,
                        connection.port,
                        connection.send_next,
                        connection.receive_next,
                        ACK,
                        WINDOW,
                        &[],
                    ));
                }
                if connection.fin_sent && connection.guest_finished && connection.send_unacked == connection.send_next {
                    finished.push((key, connection.id, connection.guest_target.to_string(), connection.port));
                }
            }
            for (key, ..) in &finished {
                if let Some(mut connection) = connections.remove(key) {
                    close_socket(&mut connection);
                }
            }
        }
        for frame in frames {
            self.send(frame);
        }
        for (_, id, ip, port) in finished {
            self.report(Event::Connect { id, ip, port, phase: "closed", blocked: false, reason: None });
        }
    }
}

fn close_socket(connection: &mut Connection) {
    connection.to_host = None;
    if let Some(socket) = connection.socket.take() {
        let _ = socket.shutdown(Shutdown::Both);
    }
}

/// A TCP segment from `source` (the address the guest is talking to) back to the guest.
#[allow(clippy::too_many_arguments)]
fn tcp_segment(
    source: Ipv4Addr,
    guest_port: u16,
    port: u16,
    sequence: u32,
    acknowledged: u32,
    flags: u8,
    window: u32,
    data: &[u8],
) -> Frame {
    let mut segment = Vec::with_capacity(20 + data.len());
    segment.extend_from_slice(&port.to_be_bytes());
    segment.extend_from_slice(&guest_port.to_be_bytes());
    segment.extend_from_slice(&sequence.to_be_bytes());
    segment.extend_from_slice(&acknowledged.to_be_bytes());
    segment.push(5 << 4); // 20-byte header
    segment.push(flags);
    segment.extend_from_slice(&(window.min(0xffff) as u16).to_be_bytes());
    segment.extend_from_slice(&0u16.to_be_bytes()); // checksum
    segment.extend_from_slice(&0u16.to_be_bytes()); // urgent pointer
    segment.extend_from_slice(data);
    let sum = transport_checksum(PROTO_TCP, source, GUEST, &segment);
    segment[16..18].copy_from_slice(&sum.to_be_bytes());
    ipv4(PROTO_TCP, source, &segment)
}

/// A DNS name, following one level of compression pointer. Returns it and where it ended.
fn read_name(message: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    for _ in 0..128 {
        let length = *message.get(at)? as usize;
        if length == 0 {
            return Some((name, at + 1));
        }
        if length & 0xc0 == 0xc0 {
            return Some((name, at + 2));
        }
        let label = message.get(at + 1..at + 1 + length)?;
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(&String::from_utf8_lossy(label));
        at += 1 + length;
    }
    None
}

fn rand_sequence() -> u32 {
    use rand::Rng;
    rand::rng().random()
}

/// The virtio device: queue 0 carries frames to the guest, queue 1 the ones it sends.
struct NetDevice {
    stack: Arc<Stack>,
}

impl Device for NetDevice {
    fn device_id(&self) -> u32 {
        1
    }

    fn features(&self) -> u64 {
        FEATURE_MAC
    }

    fn config(&self) -> Vec<u8> {
        GUEST_MAC.to_vec()
    }

    fn attach_waker(&mut self, waker: Waker) {
        *self.stack.waker.lock().unwrap() = waker;
    }

    fn notify(&mut self, queue_index: u16, queue: &mut Queue, memory: &SharedMemory) -> Result<Vec<u32>> {
        if queue_index != 1 {
            return Ok(Vec::new());
        }
        let mut irqs = Vec::new();
        while let Some(chain) = queue.pop()? {
            let mut frame = Vec::new();
            for buffer in &chain.buffers {
                if buffer.writable {
                    continue;
                }
                if let Some(bytes) = crate::machine::guest_bytes(memory, buffer.address as u32, buffer.length) {
                    frame.extend_from_slice(&bytes);
                }
            }
            if frame.len() > NET_HEADER {
                self.stack.receive_frame(&frame[NET_HEADER..]);
            }
            irqs.push(queue.release(chain, 0)?);
        }
        Ok(irqs)
    }

    fn poll(&mut self, queues: &mut [Option<Queue>], memory: &SharedMemory) -> Result<Vec<u32>> {
        let Some(Some(queue)) = queues.first_mut() else { return Ok(Vec::new()) };
        let mut irqs = Vec::new();
        loop {
            let frame = {
                let mut pending = self.stack.pending.lock().unwrap();
                match pending.frames.pop_front() {
                    Some(frame) => frame,
                    None => break,
                }
            };
            let Some(chain) = queue.pop()? else {
                self.stack.pending.lock().unwrap().frames.push_front(frame);
                break;
            };
            // virtio_net_hdr_v1, all zero but for the one buffer this frame uses.
            let mut packet = vec![0u8; NET_HEADER];
            packet[10] = 1;
            packet.extend_from_slice(&frame);
            let mut written = 0usize;
            for buffer in &chain.buffers {
                if !buffer.writable || written >= packet.len() {
                    continue;
                }
                let take = (buffer.length as usize).min(packet.len() - written);
                written += write_bytes(memory, buffer.address, &packet[written..written + take]);
            }
            irqs.push(queue.release(chain, written as u32)?);
        }
        Ok(irqs)
    }

    fn reset(&mut self) {
        self.stack.pending.lock().unwrap().frames.clear();
    }
}

