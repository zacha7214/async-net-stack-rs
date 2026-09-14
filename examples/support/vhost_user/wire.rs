//! Bounded, nonblocking vhost-user framing with SCM_RIGHTS ownership.
use super::{bad, Result};
use std::collections::VecDeque;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

pub struct Message {
    pub request: u32,
    pub flags: u32,
    pub body: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}
impl Message {
    pub fn check(&self, bytes: usize, fds: usize) -> Result<()> {
        if self.body.len() != bytes || self.fds.len() != fds {
            return Err(bad(format!(
                "request {}: expected {bytes} bytes/{fds} fds, got {}/{}",
                self.request,
                self.body.len(),
                self.fds.len()
            )));
        }
        Ok(())
    }
}
pub fn u32_at(b: &[u8], n: usize) -> u32 {
    u32::from_ne_bytes(b[n..n + 4].try_into().unwrap())
}
pub fn u64_at(b: &[u8], n: usize) -> u64 {
    u64::from_ne_bytes(b[n..n + 8].try_into().unwrap())
}

pub struct Wire {
    pub stream: UnixStream,
    header: [u8; 12],
    header_have: usize,
    body: Vec<u8>,
    body_have: usize,
    fds: Vec<OwnedFd>,
    outgoing: VecDeque<Vec<u8>>,
    sent: usize,
    pub eof: bool,
}
impl Wire {
    pub fn new(stream: UnixStream) -> Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            header: [0; 12],
            header_have: 0,
            body: Vec::new(),
            body_have: 0,
            fds: Vec::new(),
            outgoing: VecDeque::new(),
            sent: 0,
            eof: false,
        })
    }
    pub fn wants_write(&self) -> bool {
        !self.outgoing.is_empty()
    }
    pub fn send(&mut self, request: u32, reply: bool, body: &[u8]) -> Result<()> {
        if body.len() > 4096 || self.outgoing.len() >= 64 {
            return Err(bad("control output queue full"));
        }
        let mut bytes = Vec::with_capacity(12 + body.len());
        bytes.extend_from_slice(&request.to_ne_bytes());
        bytes.extend_from_slice(&(if reply { 5u32 } else { 1u32 }).to_ne_bytes());
        bytes.extend_from_slice(&(body.len() as u32).to_ne_bytes());
        bytes.extend_from_slice(body);
        self.outgoing.push_back(bytes);
        self.flush()
    }
    pub fn flush(&mut self) -> Result<()> {
        while let Some(bytes) = self.outgoing.front() {
            match self.stream.write(&bytes[self.sent..]) {
                Ok(0) => return Err(bad("control socket closed while writing")),
                Ok(n) => self.sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
            if self.sent == bytes.len() {
                self.outgoing.pop_front();
                self.sent = 0;
            }
        }
        Ok(())
    }
    pub fn receive(&mut self) -> Result<Option<Message>> {
        loop {
            if self.header_have == 12 && self.body_have == self.body.len() {
                let request = u32_at(&self.header, 0);
                let flags = u32_at(&self.header, 4);
                if flags & !9 != 0 || flags & 3 != 1 {
                    return Err(bad("bad vhost-user request flags/version"));
                }
                self.header_have = 0;
                self.body_have = 0;
                return Ok(Some(Message {
                    request,
                    flags,
                    body: std::mem::take(&mut self.body),
                    fds: std::mem::take(&mut self.fds),
                }));
            }
            let header = self.header_have < 12;
            let target = if header {
                &mut self.header[self.header_have..]
            } else {
                &mut self.body[self.body_have..]
            };
            let mut iov = libc::iovec {
                iov_base: target.as_mut_ptr().cast(),
                iov_len: target.len(),
            };
            // usize storage gives cmsghdr the required native alignment.
            let mut ancillary = [0usize; 64];
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = ancillary.as_mut_ptr().cast();
            msg.msg_controllen = std::mem::size_of_val(&ancillary) as _;
            let n = unsafe { libc::recvmsg(self.stream.as_raw_fd(), &mut msg, 0) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    return Ok(None);
                }
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e.into());
            }
            // Collect ownership before rejecting truncation or an unexpected
            // control message, so every received fd is closed on error.
            let mut unexpected = false;
            unsafe {
                let mut c = libc::CMSG_FIRSTHDR(&msg);
                while !c.is_null() {
                    if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                        let base = libc::CMSG_LEN(0) as usize;
                        let len = (*c).cmsg_len as usize;
                        if len < base || (len - base) % std::mem::size_of::<libc::c_int>() != 0 {
                            return Err(bad("malformed SCM_RIGHTS"));
                        }
                        for i in 0..(len - base) / std::mem::size_of::<libc::c_int>() {
                            let fd = libc::CMSG_DATA(c)
                                .cast::<libc::c_int>()
                                .add(i)
                                .read_unaligned();
                            self.fds.push(OwnedFd::from_raw_fd(fd));
                        }
                    } else {
                        unexpected = true;
                    }
                    c = libc::CMSG_NXTHDR(&msg, c);
                }
            }
            if unexpected
                || msg.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0
                || self.fds.len() > 8
            {
                return Err(bad("unexpected/truncated ancillary data or too many fds"));
            }
            for fd in &self.fds {
                if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                    return Err(io::Error::last_os_error().into());
                }
            }
            if n == 0 {
                self.eof = true;
                if self.header_have != 0 || self.body_have != 0 || !self.fds.is_empty() {
                    return Err(bad("truncated control message"));
                }
                return Ok(None);
            }
            if header {
                self.header_have += n as usize;
                if self.header_have == 12 {
                    let size = u32_at(&self.header, 8) as usize;
                    if size > 4096 {
                        return Err(bad("control payload exceeds 4096 bytes"));
                    }
                    self.body.resize(size, 0);
                }
            } else {
                self.body_have += n as usize;
            }
        }
    }
}
