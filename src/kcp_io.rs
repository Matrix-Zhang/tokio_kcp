use std::cell::RefCell;
use std::cmp;
use std::io::{self, BufRead, Read, Write};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use kcp::Error as KcpError;
use mio::{self, Evented, PollOpt, Ready, Registration, SetReadiness, Token};

use session::{KcpClientSession, KcpClientSessionUpdater, KcpServerSession, KcpServerSessionUpdater, KcpSession,
              KcpSessionMode};
use skcp::SharedKcp;

/// Base Io object for KCP
struct KcpIo {
    kcp: SharedKcp,
    read_buf: Vec<u8>,
    read_pos: usize,
    read_cap: usize,
}

impl KcpIo {
    pub fn new(kcp: SharedKcp) -> KcpIo {
        KcpIo {
            kcp: kcp,
            read_buf: Vec::new(),
            read_pos: 0,
            read_cap: 0,
        }
    }

    /// Call everytime you got data from transmission
    pub fn input(&mut self, buf: &[u8]) -> io::Result<()> {
        self.kcp.input(buf).map_err(From::from)
    }

    /// MTU
    pub fn mtu(&self) -> usize {
        self.kcp.mtu()
    }

    /// Shutdown
    pub fn shutdown(&mut self) -> io::Result<()> {
        self.kcp.close();
        Ok(())
    }

    /// Check if can read
    pub fn can_read(&self) -> bool {
        self.kcp.can_read()
    }

    fn buf_remaining(&self) -> usize {
        self.read_cap - self.read_pos
    }
}

impl BufRead for KcpIo {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.read_pos >= self.read_cap {
            if self.read_buf.is_empty() {
                let mtu = self.kcp.mtu();
                trace!("[INIT] KcpIo mtu {}", mtu);
                self.read_buf.resize(mtu, 0);
            }

            loop {
                let n = match self.kcp.recv(&mut self.read_buf) {
                    Ok(n) => n,
                    Err(KcpError::UserBufTooSmall) => {
                        let orig = self.read_buf.len();
                        let incr = self.kcp.peeksize().next_power_of_two();
                        trace!("[RECV] kcp.recv buf too small, {} -> {}", orig, incr);
                        self.read_buf.resize(incr, 0);
                        continue;
                    }
                    Err(err) => {
                        return Err(From::from(err));
                    }
                };

                self.read_pos = 0;
                self.read_cap = n;

                trace!("[RECV] kcp.recv size={} {:?}", n, ::debug::BsDebug(&self.read_buf[..n]));

                break;
            }
        }

        Ok(&self.read_buf[self.read_pos..self.read_cap])
    }

    fn consume(&mut self, amt: usize) {
        self.read_pos = cmp::min(self.read_cap, self.read_pos + amt);
    }
}

impl Read for KcpIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.buf_remaining() == 0 {
            // Try to read directly
            match self.kcp.recv(buf) {
                Ok(n) => {
                    trace!("[RECV] KcpIo.read directly size={}", n);
                    return Ok(n);
                }
                Err(KcpError::UserBufTooSmall) => {
                    trace!("[RECV] KcpIo.read directly peeksize={} buf size={} too small",
                           self.kcp.peeksize(),
                           buf.len());
                }
                Err(err) => return Err(From::from(err)),
            }
        }

        let nread = {
            let mut available = self.fill_buf()?;
            available.read(buf)?
        };
        self.consume(nread);
        trace!("[RECV] KcpIo.read size={}", nread);
        Ok(nread)
    }
}

impl Write for KcpIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        trace!("[SEND] KcpIo.write size={} {:?}", buf.len(), ::debug::BsDebug(buf));
        let n = self.kcp.send(buf)?;
        trace!("[SEND] kcp.send size={}", n);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.kcp.flush()?;
        trace!("[SEND] Flushed KCP buffer");
        Ok(())
    }
}

impl Drop for KcpIo {
    fn drop(&mut self) {
        self.kcp.close()
    }
}

/// Io object for client
///
/// It doesn't need to have evented to be implemented
pub struct ClientKcpIo {
    io: KcpIo,
}

impl ClientKcpIo {
    pub fn new(kcp: SharedKcp,
               addr: SocketAddr,
               expire_dur: Duration,
               u: &mut KcpClientSessionUpdater)
               -> io::Result<ClientKcpIo> {
        let mut sess = KcpSession::new(kcp.clone(), addr, expire_dur, KcpSessionMode::Client)?;
        sess.update()?; // Call update once it is created
        let sess = Rc::new(RefCell::new(sess));
        let sess = KcpClientSession::new(sess);

        u.insert_by_conv(kcp.conv(), sess);

        trace!("[CLIENT] Created stream bind addr={} conv={}", addr, kcp.conv());
        Ok(ClientKcpIo { io: KcpIo::new(kcp) })
    }

    pub fn input(&mut self, buf: &[u8]) -> io::Result<()> {
        self.io.input(buf)
    }

    pub fn mtu(&self) -> usize {
        self.io.mtu()
    }
}

impl Read for ClientKcpIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io.read(buf)
    }
}

impl Write for ClientKcpIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

/// Io object for server
///
/// It implements `Evented` apis
pub struct ServerKcpIo {
    io: KcpIo,
    reg: Registration,
    readiness: SetReadiness,
}

impl ServerKcpIo {
    pub fn new(kcp: SharedKcp,
               addr: SocketAddr,
               expire_dur: Duration,
               u: &mut KcpServerSessionUpdater)
               -> io::Result<ServerKcpIo> {
        let sess = KcpSession::new_shared(kcp.clone(), addr, expire_dur, KcpSessionMode::Server)?;
        let (reg, r) = Registration::new2();
        let sess = KcpServerSession::new(sess, r.clone());

        u.insert_by_conv(kcp.conv(), sess);

        Ok(ServerKcpIo {
               io: KcpIo::new(kcp),
               reg: reg,
               readiness: r,
           })
    }

    /// Call everytime you got data from transmission
    pub fn input(&mut self, data: &[u8]) -> io::Result<()> {
        self.io.input(data)?;
        if self.io.can_read() {
            self.readiness.set_readiness(Ready::readable())?;
        }
        Ok(())
    }

    /// Close it
    pub fn shutdown(&mut self) -> io::Result<()> {
        self.io.shutdown()
    }
}

impl Evented for ServerKcpIo {
    fn register(&self, poll: &mio::Poll, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()> {
        self.reg.register(poll, token, interest, opts)
    }

    fn reregister(&self, poll: &mio::Poll, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()> {
        self.reg.reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
        <Registration as Evented>::deregister(&self.reg, poll)
    }
}

impl Read for ServerKcpIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io.read(buf)
    }
}

impl Write for ServerKcpIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}
