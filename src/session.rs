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
    sessions: Rc<RefCell<HashMap<u32, KcpServerSession>>>,
    alloc_conv: u32,
}

impl Debug for KcpSessionUpdater {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let hmap = self.sessions.borrow();
        write!(f, "KcpSessionUpdater {{ sessions: {:?} }}", &*hmap)
    }
}

impl KcpSessionUpdater {
    pub fn new() -> KcpSessionUpdater {
        KcpSessionUpdater {
            sessions: Rc::new(RefCell::new(HashMap::new())),
            alloc_conv: 1,
        }
    }
}

impl KcpSessionUpdater {
    fn sessions_mut(&mut self) -> cell::RefMut<HashMap<u32, KcpServerSession>> {
        self.sessions.borrow_mut()
    }

    pub fn input_by_conv(&mut self, conv: u32, buf: &mut [u8]) -> io::Result<bool> {
        match self.sessions_mut().get_mut(&conv) {
            None => Ok(false),
            Some(session) => {
                session.input(buf)?;
                Ok(true)
            }
        }
    }

    pub fn remove_by_conv(&mut self, conv: u32) {
        let mut ses = self.sessions_mut();
        ses.remove(&conv);
    }

    pub fn insert_by_conv(&mut self, conv: u32, s: KcpServerSession) {
        let mut ses = self.sessions_mut();
        ses.insert(conv, s);
    }

    /// Get one unused `conv`
    pub fn get_free_conv(&mut self) -> u32 {
        let ses = self.sessions.borrow();

        let mut conv = self.alloc_conv;
        while ses.contains_key(&conv) {
            let (c, _) = self.alloc_conv.overflowing_add(1);
            self.alloc_conv = c;
            if self.alloc_conv == 0 {
                self.alloc_conv = 1;
            }

            conv = self.alloc_conv;
        }

        conv
    }
}

/// Shared session for controlling from other objects
pub type SharedKcpSession = Rc<RefCell<KcpSession>>;

/// KCP session mode
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub enum KcpSessionMode {
    Client,
    Server,
}

/// Session of a KCP conversation
pub struct KcpSession {
    kcp: SharedKcp,
    timer: Timeout,
    addr: SocketAddr,
    expire_dur: Duration,
    mode: KcpSessionMode,
}

impl KcpSession {
    pub fn new(kcp: SharedKcp,
               addr: SocketAddr,
               expire_dur: Duration,
               handle: &Handle,
               mode: KcpSessionMode)
               -> io::Result<KcpSession> {
        let n = KcpSession {
            kcp: kcp,
            timer: Timeout::new(Duration::from_secs(0), handle)?,
            addr: addr,
            expire_dur: expire_dur,
            mode: mode,
        };
        Ok(n)
    }

    pub fn new_shared(kcp: SharedKcp,
                      addr: SocketAddr,
                      expire_dur: Duration,
                      handle: &Handle,
                      mode: KcpSessionMode)
                      -> io::Result<SharedKcpSession> {
        let sess = KcpSession::new(kcp, addr, expire_dur, handle, mode)?;
        Ok(Rc::new(RefCell::new(sess)))
    }

    /// Get peer addr
    pub fn addr(&self) -> &SocketAddr {
        &self.addr
    }

    /// Called when you received a packet
    pub fn input(&mut self, buf: &[u8]) -> io::Result<()> {
        trace!("[SESS] input size={} addr={} {:?}", buf.len(), self.addr, ::debug::BsDebug(buf));
        self.kcp.input(buf)?;
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
        !self.kcp.has_waitsnd() // Does not have anything to be sent
            && self.kcp.elapsed() > Duration::from_secs(10) // Wait for 10s
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
        self.kcp.try_notify_writable();

        // Check if it is closed
        if self.is_closed() {
            if self.mode == KcpSessionMode::Client {
                // Take over the UDP's control
                // Because in client mode, when the Stream is closed, session is the only one to be responsible
                // for receving data from udp and input to kcp.
                self.kcp.fetch()?;
            }

            if self.can_close() {
                trace!("[SESS] addr={} conv={} closing", self.addr, self.kcp.conv());
                return Ok(Async::Ready(None));
            }
        }

        Ok(Async::Ready(Some(())))
    }

    /// Check if it is readable
    pub fn can_read(&self) -> bool {
        self.kcp.can_read()
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
            u.insert_by_conv(mss.kcp.conv(), sess.clone());
        }
        sess
    }

    /// Calls when you got data from transmission
    pub fn input(&mut self, buf: &[u8]) -> io::Result<()> {
        let mut sess = self.session.borrow_mut();
        sess.input(buf)?;

        // Now we have put data into KCP
        // So it is time to try `recv`. But it may failed, because the inputted data may be an ACK packet.
        if sess.can_read() {
            self.readiness.set_readiness(Ready::readable())
        } else {
            Ok(())
        }
    }
}

impl Stream for KcpServerSession {
    type Item = ();
    type Error = io::Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut sess = self.session.borrow_mut();
        match sess.poll() {
            Err(err) => {
                // Session is closed, remove itself from updater
                self.updater.remove_by_conv(sess.kcp.conv());
                trace!("[SESS] Close and remove addr={} conv={}, err: {}", sess.addr, sess.kcp.conv(), err);

                // Awake pending reads
                self.readiness.set_readiness(Ready::readable())?;
                Err(err)
            }
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(Some(x))) => Ok(Async::Ready(Some(x))),
            Ok(Async::Ready(None)) => {
                // Session is closed, remove itself from updater
                self.updater.remove_by_conv(sess.kcp.conv());
                trace!("[SESS] Close and remove addr={} conv={}", sess.addr, sess.kcp.conv());

                // Awake pending reads
                self.readiness.set_readiness(Ready::readable())?;
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
