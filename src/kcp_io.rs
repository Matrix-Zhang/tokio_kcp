use std::cell::RefCell;
use std::cmp;
use std::io::{self, BufRead, Read, Write};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use futures::{Future, Stream};
use kcp::Error as KcpError;
use mio::{self, Evented, PollOpt, Ready, Registration, SetReadiness, Token};
use tokio_core::reactor::Handle;

use session::{KcpClientSession, KcpServerSession, KcpSession, KcpSessionMode, KcpSessionUpdater};
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
        let mtu = kcp.mtu();
        trace!("[INIT] KcpIo mtu {}", mtu);
        let mut buf = Vec::with_capacity(mtu);
        unsafe {
            buf.set_len(mtu);
        }

        KcpIo {
            kcp: kcp,
            read_buf: buf,
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
}

impl BufRead for KcpIo {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.read_pos >= self.read_cap {
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
    pub fn new(kcp: SharedKcp, addr: SocketAddr, expire_dur: Duration, handle: &Handle) -> io::Result<ClientKcpIo> {
        let mut sess = KcpSession::new(kcp.clone(), addr, expire_dur, handle, KcpSessionMode::Client)?;
        sess.update()?; // Call update once it is created
        let sess = Rc::new(RefCell::new(sess));
        let sess = KcpClientSession::new(sess);
        handle.spawn(sess.for_each(|_| Ok(())).map_err(|err| {
                                                           error!("Failed to update KCP session: err: {:?}", err);
                                                       }));
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
               handle: &Handle,
               u: &mut KcpSessionUpdater)
               -> io::Result<ServerKcpIo> {
        let sess = KcpSession::new_shared(kcp.clone(), addr, expire_dur, handle, KcpSessionMode::Server)?;
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

// impl KcpIo {
//     pub fn new(shared_kcp: SharedKcp,
//                addr: SocketAddr,
//                handle: &Handle,
//                owner: Option<KcpSessionUpdater>,
//                expire_dur: Duration)
//                -> io::Result<KcpIo> {
//         let (registration, readiness) = Registration::new2();
//         let timer = Timeout::new_at(Instant::now(), handle)?;

//         let elapsed = Rc::new(RefCell::new(Instant::now()));
//         let close_flag = Rc::new(RefCell::new(false));
//         let session = SharedKcpSession::new(shared_kcp.clone(),
//                                             timer,
//                                             elapsed.clone(),
//                                             readiness.clone(),
//                                             addr,
//                                             owner,
//                                             close_flag.clone(),
//                                             expire_dur)?;
//         handle.spawn(session.for_each(|_| Ok(())).map_err(|err| {
//                                                               error!("Failed to update KCP session: err: {:?}", err);
//                                                           }));

//         Ok(KcpIo {
//                kcp: shared_kcp,
//                registration: registration,
//                readiness: readiness,
//                last_update: elapsed,
//                close_flag: close_flag,
//                read_buf: vec![0u8; 65535],
//                read_pos: 0,
//                read_cap: 0,
//            })
//     }

//     pub fn input_buf(&mut self, buf: &[u8]) -> io::Result<()> {
//         {
//             let mut last_update = self.last_update.borrow_mut();
//             *last_update = Instant::now();
//             let mut kcp = self.kcp.borrow_mut();
//             kcp.input(buf)?;
//             trace!("[INPUT] KcpIo.input size={}", buf.len());
//         }
//         self.set_readable()
//     }

//     fn set_readable(&mut self) -> io::Result<()> {
//         self.readiness.set_readiness(Ready::readable())
//     }

//     pub fn can_read(&self) -> io::Result<bool> {
//         let kcp = self.kcp.borrow();
//         kcp.peeksize().map(|n| n != 0)
//     }
// }



// impl Evented for KcpIo {
//     fn register(&self, poll: &mio::Poll, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()> {
//         self.registration.register(poll, token, interest, opts)
//     }

//     fn reregister(&self, poll: &mio::Poll, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()> {
//         self.registration.reregister(poll, token, interest, opts)
//     }

//     fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
//         <Registration as Evented>::deregister(&self.registration, poll)
//     }
// }
