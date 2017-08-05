use std::cell::RefCell;
use std::cmp;
use std::io::{self, BufRead, Read, Write};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures::{Future, Stream};
use mio::{self, Evented, PollOpt, Ready, Registration, SetReadiness, Token};
use tokio_core::reactor::{Handle, Timeout};

use session::{KcpSessionUpdater, SharedKcpSession};
use skcp::SharedKcp;

pub struct KcpIo {
    kcp: SharedKcp,
    registration: Registration,
    readiness: SetReadiness,
    last_update: Rc<RefCell<Instant>>,
    close_flag: Rc<RefCell<bool>>,
    read_buf: Vec<u8>,
    read_pos: usize,
    read_cap: usize,
}

impl Drop for KcpIo {
    fn drop(&mut self) {
        let mut cf = self.close_flag.borrow_mut();
        *cf = true;
    }
}

impl KcpIo {
    pub fn new(shared_kcp: SharedKcp,
               addr: SocketAddr,
               handle: &Handle,
               owner: Option<KcpSessionUpdater>,
               expire_dur: Duration)
               -> io::Result<KcpIo> {
        let (registration, readiness) = Registration::new2();
        let timer = Timeout::new_at(Instant::now(), handle)?;

        let elapsed = Rc::new(RefCell::new(Instant::now()));
        let close_flag = Rc::new(RefCell::new(false));
        let session = SharedKcpSession::new(shared_kcp.clone(),
                                            timer,
                                            elapsed.clone(),
                                            readiness.clone(),
                                            addr,
                                            owner,
                                            close_flag.clone(),
                                            expire_dur)?;
        handle.spawn(session.for_each(|_| Ok(())).map_err(|err| {
                                                              error!("Failed to update KCP session: err: {:?}", err);
                                                          }));

        Ok(KcpIo {
               kcp: shared_kcp,
               registration: registration,
               readiness: readiness,
               last_update: elapsed,
               close_flag: close_flag,
               read_buf: vec![0u8; 65535],
               read_pos: 0,
               read_cap: 0,
           })
    }

    pub fn input_buf(&mut self, buf: &[u8]) -> io::Result<()> {
        let n = {
            let mut last_update = self.last_update.borrow_mut();
            *last_update = Instant::now();
            let mut kcp = self.kcp.borrow_mut();
            kcp.input(buf)?
        };
        self.set_readable()?;
        Ok(n)
    }

    fn set_readable(&mut self) -> io::Result<()> {
        self.readiness.set_readiness(Ready::readable())
    }

    pub fn can_read(&self) -> io::Result<bool> {
        let kcp = self.kcp.borrow();
        kcp.peeksize().map(|n| n != 0)
    }
}

impl BufRead for KcpIo {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.read_pos >= self.read_cap {
            let n = self.kcp.borrow_mut().recv(&mut self.read_buf)?;
            self.read_pos = 0;
            self.read_cap = n;
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
        Ok(nread)
    }
}

impl Write for KcpIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut kcp = self.kcp.borrow_mut();
        kcp.send(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut kcp = self.kcp.borrow_mut();
        kcp.flush()
    }
}

impl Evented for KcpIo {
    fn register(&self, poll: &mio::Poll, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()> {
        self.registration.register(poll, token, interest, opts)
    }

    fn reregister(&self, poll: &mio::Poll, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()> {
        self.registration.reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
        <Registration as Evented>::deregister(&self.registration, poll)
    }
}
