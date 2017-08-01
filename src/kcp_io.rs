use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Instant;

use bytes::BytesMut;
use bytes::buf::FromBuf;
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
}

impl KcpIo {
    pub fn new(shared_kcp: SharedKcp,
               addr: SocketAddr,
               handle: &Handle,
               owner: Option<KcpSessionUpdater>)
               -> io::Result<KcpIo> {
        let (registration, readiness) = Registration::new2();
        let timer = Timeout::new_at(Instant::now(), handle)?;

        let elapsed = Rc::new(RefCell::new(Instant::now()));
        let session =
            SharedKcpSession::new(shared_kcp.clone(), timer, elapsed.clone(), readiness.clone(), addr, owner)?;
        handle.spawn(session.for_each(|_| Ok(())).map_err(|err| {
                                                              error!("Failed to update KCP session: err: {:?}", err);
                                                          }));

        Ok(KcpIo {
               kcp: shared_kcp,
               registration: registration,
               readiness: readiness,
               last_update: elapsed,
           })
    }

    pub fn input_buf(&mut self, buf: &[u8]) -> io::Result<()> {
        {
            let mut last_update = self.last_update.borrow_mut();
            *last_update = Instant::now();
            let mut kcp = self.kcp.borrow_mut();
            kcp.input(&mut BytesMut::from_buf(buf))?;
        }
        self.set_readable()
    }

    pub fn set_writable(&mut self) -> io::Result<()> {
        self.readiness.set_readiness(Ready::writable())
    }

    pub fn set_readable(&mut self) -> io::Result<()> {
        self.readiness.set_readiness(Ready::readable())
    }
}

impl Read for KcpIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.kcp.borrow_mut().recv(buf)
    }
}

impl Write for KcpIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut kcp = self.kcp.borrow_mut();
        kcp.send(buf).and_then(|n| kcp.flush().map(|_| n))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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
