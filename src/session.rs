use std::cell::{self, RefCell};
use std::collections::HashMap;
use std::fmt::{self, Debug};
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use futures::{Async, Future, Poll, Stream};
use mio::{Ready, SetReadiness};
use tokio_core::reactor::{Handle, Timeout};

use skcp::SharedKcp;

#[derive(Clone)]
pub struct KcpSessionUpdater {
    sessions: Rc<RefCell<HashMap<SocketAddr, KcpServerSession>>>,
}

impl Debug for KcpSessionUpdater {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let hmap = self.sessions.borrow();
        write!(f, "KcpSessionUpdater {{ sessions: {:?} }}", &*hmap)
    }
}

impl KcpSessionUpdater {
    pub fn new() -> KcpSessionUpdater {
        KcpSessionUpdater { sessions: Rc::new(RefCell::new(HashMap::new())) }
    }
}

impl KcpSessionUpdater {
    fn sessions_mut(&mut self) -> cell::RefMut<HashMap<SocketAddr, KcpServerSession>> {
        self.sessions.borrow_mut()
    }

    pub fn input_by_addr(&mut self, addr: &SocketAddr, buf: &mut [u8]) -> io::Result<bool> {
        match self.sessions_mut().get_mut(addr) {
            None => Ok(false),
            Some(session) => {
                session.input(buf)?;
                Ok(true)
            }
        }
    }

    pub fn remove_by_addr(&mut self, addr: &SocketAddr) {
        let mut ses = self.sessions_mut();
        ses.remove(addr);
    }

    pub fn insert_by_addr(&mut self, addr: SocketAddr, s: KcpServerSession) {
        let mut ses = self.sessions_mut();
        ses.insert(addr, s);
    }
}

/// Shared session for controlling from other objects
pub type SharedKcpSession = Rc<RefCell<KcpSession>>;

/// Session of a KCP conversation
pub struct KcpSession {
    kcp: SharedKcp,
    timer: Timeout,
    addr: SocketAddr,
    expire_dur: Duration,
}

impl KcpSession {
    pub fn new(kcp: SharedKcp, addr: SocketAddr, expire_dur: Duration, handle: &Handle) -> io::Result<KcpSession> {
        let n = KcpSession {
            kcp: kcp,
            timer: Timeout::new(Duration::from_secs(0), handle)?,
            addr: addr,
            expire_dur: expire_dur,
        };
        Ok(n)
    }

    pub fn new_shared(kcp: SharedKcp,
                      addr: SocketAddr,
                      expire_dur: Duration,
                      handle: &Handle)
                      -> io::Result<SharedKcpSession> {
        let sess = KcpSession::new(kcp, addr, expire_dur, handle)?;
        Ok(Rc::new(RefCell::new(sess)))
    }

    /// Get peer addr
    pub fn addr(&self) -> &SocketAddr {
        &self.addr
    }

    /// Called when you received a packet
    pub fn input(&mut self, buf: &[u8]) -> io::Result<()> {
        self.kcp.input(buf)?;
        trace!("[SESS] input size={} addr={} {:?}", buf.len(), self.addr, ::debug::BsDebug(buf));
        Ok(())
    }

    /// Check if session pending too long
    pub fn is_expired(&self) -> bool {
        self.kcp.elapsed() > self.expire_dur
    }

    /// Called every tick
    pub fn update(&mut self) -> io::Result<()> {
        let next = self.kcp.update()?;
        self.timer.reset(next);
        Ok(())
    }

    /// Called if it is expired
    pub fn expire(&mut self) -> io::Result<()> {
        self.kcp.set_expired()?;
        trace!("[SESS] addr={} is expired", self.addr);
        Ok(())
    }

    /// Check if session is closed
    pub fn is_closed(&self) -> bool {
        self.kcp.is_closed()
    }

    /// Check if it is ready to close
    pub fn can_close(&self) -> bool {
        !self.kcp.has_waitsnd()
    }

    /// Pull like a stream
    pub fn poll(&mut self) -> Poll<Option<()>, io::Error> {
        try_ready!(self.timer.poll());

        // Session is already expired, drop this session
        if self.is_expired() {
            self.expire()?;
            return Ok(Async::Ready(None));
        }

        // Update it
        self.update()?;

        // Check if it is closed
        if self.is_closed() && self.can_close() {
            trace!("[SESS] addr={} closed", self.addr);
            return Ok(Async::Ready(None));
        }

        Ok(Async::Ready(Some(())))
    }
}

/// Server accepted session
///
/// This session is updated by the accept function. So it requires to notify readable events when it has
/// data to read.
#[derive(Clone)]
pub struct KcpServerSession {
    session: SharedKcpSession,
    readiness: SetReadiness,
    updater: KcpSessionUpdater,
}

impl KcpServerSession {
    pub fn new(sess: SharedKcpSession, r: SetReadiness, u: &mut KcpSessionUpdater) -> KcpServerSession {
        let sess = KcpServerSession {
            session: sess,
            readiness: r,
            updater: u.clone(),
        };

        // Register it
        {
            let mss = sess.session.borrow();
            u.insert_by_addr(*mss.addr(), sess.clone());
        }
        sess
    }

    /// Calls when you got data from transmission
    pub fn input(&mut self, buf: &[u8]) -> io::Result<()> {
        let mut sess = self.session.borrow_mut();
        sess.input(buf)?;

        // Now we have put data into KCP
        // So it is time to try `recv`. But it may failed, because the inputted data may be an ACK packet.
        self.readiness.set_readiness(Ready::readable())
    }
}

impl Stream for KcpServerSession {
    type Item = ();
    type Error = io::Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut sess = self.session.borrow_mut();
        match sess.poll() {
            Err(err) => Err(err),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(Some(x))) => Ok(Async::Ready(Some(x))),
            Ok(Async::Ready(None)) => {
                // Session is closed, remove itself from updater
                let sess = self.session.borrow();
                self.updater.remove_by_addr(sess.addr());
                Ok(Async::Ready(None))
            }
        }
    }
}

impl Debug for KcpServerSession {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let sess = self.session.borrow();
        write!(f, "KcpServerSession({})", sess.addr())
    }
}

/// Client session
///
/// This session is not requires to notify any events, because all of them is done on the caller side.
#[derive(Clone)]
pub struct KcpClientSession {
    session: SharedKcpSession,
}

impl KcpClientSession {
    pub fn new(sess: SharedKcpSession) -> KcpClientSession {
        KcpClientSession { session: sess }
    }
}

impl Stream for KcpClientSession {
    type Item = ();
    type Error = io::Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut sess = self.session.borrow_mut();
        sess.poll()
    }
}
