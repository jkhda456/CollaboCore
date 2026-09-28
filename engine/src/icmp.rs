//! One ICMP echo from this computer, for the guest's `ping` (net.rs).
//!
//! No privileges needed anywhere: an unprivileged ICMP datagram socket on Linux (allowed by
//! `net.ipv4.ping_group_range`, which desktop distributions open to everyone) and macOS, and
//! IcmpSendEcho on Windows. Where the system does not allow it, the ping fails with that reason
//! instead of pretending.
use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::Result;

/// A reply: how long it took. None: no reply within the timeout.
pub struct Reply {
    pub rtt: Duration,
}

#[cfg(unix)]
pub fn echo(address: Ipv4Addr, payload: &[u8], timeout: Duration) -> Result<Option<Reply>> {
    use anyhow::bail;
    use std::time::Instant;

    // SAFETY: plain socket calls on a descriptor this function owns and closes.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP);
        if fd < 0 {
            bail!("this computer does not allow unprivileged ping ({})", std::io::Error::last_os_error());
        }
        struct Close(i32);
        impl Drop for Close {
            fn drop(&mut self) {
                unsafe { libc::close(self.0) };
            }
        }
        let _close = Close(fd);

        // Echo request: type 8, code 0, checksum, identifier, sequence, data. Linux sets the
        // identifier (the socket's "port") and the checksum itself; macOS wants the checksum.
        let sequence: u16 = rand::random();
        let mut request = vec![8u8, 0, 0, 0, 0, 0];
        request.extend_from_slice(&sequence.to_be_bytes());
        request.extend_from_slice(payload);
        let sum = checksum(&request);
        request[2..4].copy_from_slice(&sum.to_be_bytes());

        let mut to: libc::sockaddr_in = std::mem::zeroed();
        to.sin_family = libc::AF_INET as _;
        to.sin_addr.s_addr = u32::from_ne_bytes(address.octets());
        let started = Instant::now();
        let sent = libc::sendto(
            fd,
            request.as_ptr().cast(),
            request.len(),
            0,
            (&to as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        if sent < 0 {
            bail!("sending the echo: {}", std::io::Error::last_os_error());
        }

        let mut buffer = [0u8; 2048];
        loop {
            let left = timeout.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Ok(None);
            }
            let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            let ready = libc::poll(&mut poll, 1, left.as_millis().max(1) as i32);
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                bail!("waiting for the echo: {error}");
            }
            if ready == 0 {
                return Ok(None);
            }
            let read = libc::recv(fd, buffer.as_mut_ptr().cast(), buffer.len(), 0);
            if read < 0 {
                bail!("reading the echo: {}", std::io::Error::last_os_error());
            }
            let mut reply = &buffer[..read as usize];
            // macOS hands back the IP header too; Linux only the ICMP message.
            if reply.first().is_some_and(|byte| byte >> 4 == 4) && reply.len() >= 20 {
                reply = &reply[((reply[0] & 0xf) as usize * 4).min(reply.len())..];
            }
            // An echo reply (type 0) to our sequence number; anything else is someone else's.
            if reply.len() >= 8 && reply[0] == 0 && reply[6..8] == sequence.to_be_bytes() {
                return Ok(Some(Reply { rtt: started.elapsed() }));
            }
        }
    }
}

#[cfg(windows)]
pub fn echo(address: Ipv4Addr, payload: &[u8], timeout: Duration) -> Result<Option<Reply>> {
    use anyhow::bail;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::NetworkManagement::IpHelper::{IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho, ICMP_ECHO_REPLY};

    // SAFETY: the handle is closed below; the reply buffer is sized as IcmpSendEcho documents
    // (one ICMP_ECHO_REPLY, the data, 8 bytes for an ICMP error) and outlives the call.
    unsafe {
        let handle = IcmpCreateFile();
        if handle == INVALID_HANDLE_VALUE {
            bail!("IcmpCreateFile failed ({})", std::io::Error::last_os_error());
        }
        let mut reply = vec![0u8; std::mem::size_of::<ICMP_ECHO_REPLY>() + payload.len() + 8 + 64];
        let replies = IcmpSendEcho(
            handle,
            u32::from_ne_bytes(address.octets()),
            payload.as_ptr().cast(),
            payload.len() as u16,
            std::ptr::null(),
            reply.as_mut_ptr().cast(),
            reply.len() as u32,
            timeout.as_millis().max(1) as u32,
        );
        let error = std::io::Error::last_os_error();
        IcmpCloseHandle(handle);
        if replies == 0 {
            // IP_REQ_TIMED_OUT (11010) is just no answer.
            return match error.raw_os_error() {
                Some(11010) => Ok(None),
                _ => bail!("IcmpSendEcho: {error}"),
            };
        }
        let echo = std::ptr::read_unaligned(reply.as_ptr().cast::<ICMP_ECHO_REPLY>());
        match echo.Status {
            0 => Ok(Some(Reply { rtt: Duration::from_millis(echo.RoundTripTime as u64) })),
            _ => Ok(None), // unreachable, TTL expired, …: no echo came back
        }
    }
}

#[cfg(unix)]
fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    for pair in data.chunks(2) {
        sum += u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)]) as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}
