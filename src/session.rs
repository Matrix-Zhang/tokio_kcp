use std::cell::RefCell;
use std::cmp::{Ord, Ordering};
use std::collections::{HashMap, HashSet};
use std::fmt::{self, Debug};
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures::{Async, Future, Poll, Stream};
use futures::task::{self, Task};
use mio::{Ready, SetReadiness};
use priority_queue::PriorityQueue;
use tokio_core::reactor::{Handle, Timeout};

use skcp::SharedKcp;

/// KCP session features
pub trait Session: Stream<Item = Instant, Error = io::Error> {
    fn input(&mut self, buf: &[u8]) -> io::Result<()>;
    fn addr(&self) -> SocketAddr;
}

#[derive(Eq, PartialEq, Copy, Clone)]
struct InstantOrd(Instant);

impl PartialOrd for InstantOrd {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match self.0.partial_cmp(&other.0) {
            Some(Ordering::Equal) => Some(Ordering::Equal),
            Some(Ordering::Greater) => Some(Ordering::Less),
            Some(Ordering::Less) => Some(Ordering::Greater),
            None => None,
        }
    }
}

impl Ord for InstantOrd {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.0.cmp(&other.0) {
            Ordering::Equal => Ordering::Equal,
            Ordering::Greater => Ordering::Less,
            Ordering::Less => Ordering::Greater,
        }
    }
}

struct KcpSessionUpdaterInner<S>
where
    S: Session,
{
    sessions: HashMap<u32, S>,
    alloc_conv: u32,
    is_stop: bool,
    timeout: Timeout,
    conv_queue: PriorityQueue<u32, InstantOrd>,
    task: Option<Task>,
    known_endpoint: HashMap<SocketAddr, HashSet<u32>>,
}

impl<S> Debug for KcpSessionUpdaterInner<S>
where
    S: Session + Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "sessions: {:?}, alloc_conv: {}", self.sessions, self.alloc_conv)
    }
}

/// Session updater for server accepted streams
pub type KcpServerSessionUpdater = KcpSessionUpdater<KcpServerSession>;
/// Session updater for clients
pub type KcpClientSessionUpdater = KcpSessionUpdater<KcpClientSession>;

/// KCP session updater
///
/// A structure that holds all created sessions and call `update` on them.
pub struct KcpSessionUpdater<S>
where
    S: Session,
{
    inner: Rc<RefCell<KcpSessionUpdaterInner<S>>>,
}

impl<S> Clone for KcpSessionUpdater<S>
where
    S: Session,
{
    fn clone(&self) -> Self {
        KcpSessionUpdater { inner: self.inner.clone() }
    }
}

impl<S> Debug for KcpSessionUpdater<S>
where
    S: Session + Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let inner = self.inner.borrow();
        write!(f, "KcpSessionUpdater {{ {:?} }}", &*inner)
    }
}

impl<S> KcpSessionUpdater<S>
where
    S: Session + 'static,
{
    /// Create a new updater and then register it to the `Core`.
    pub fn new(handle: &Handle) -> io::Result<KcpSessionUpdater<S>> {
        let timeout = Timeout::new(Duration::from_secs(0), handle)?;

        let u = KcpSessionUpdater {
            inner: Rc::new(RefCell::new(KcpSessionUpdaterInner {
                                            sessions: HashMap::new(),
                                            alloc_conv: 0,
                                            is_stop: false,
                                            timeout: timeout,
                                            conv_queue: PriorityQueue::new(),
                                            task: None,
                                            known_endpoint: HashMap::new(),
                                        })),
        };

        let run_it = u.clone();
        handle.spawn(run_it.for_each(|_| Ok(())).map_err(|err| {
                                                             error!("Session update failed! Err: {:?}", err);
                                                         }));

        Ok(u)
    }

    #[doc(hidden)]
    pub fn input_by_conv(&mut self, conv: u32, endpoint: &SocketAddr, buf: &mut [u8]) -> io::Result<bool> {
        let mut inner = self.inner.borrow_mut();

        if conv == 0 {
            // Ask for allocating??
            // So we are in server mode, each endpoint to be paired with one conv
            if inner.known_endpoint.contains_key(endpoint) {
                trace!("[SESS] addr={} with conv=0 retransmitted", endpoint);
                return Ok(true);
            }
        }

        match inner.sessions.get_mut(&conv) {
            None => Ok(false),
            Some(session) => {
                session.input(buf)?;
                Ok(true)
            }
        }
    }

    #[doc(hidden)]
    pub fn insert_by_conv(&mut self, conv: u32, s: S) {
        let endpoint = s.addr();

        let mut inner = self.inner.borrow_mut();
        inner.sessions.insert(conv, s);
        inner.conv_queue.push(conv, InstantOrd(Instant::now()));

        {
            let mut convs = inner.known_endpoint
                                 .entry(endpoint)
                                 .or_insert(HashSet::new());
            convs.insert(conv);
        }

        if let Some(task) = inner.task.take() {
            task.notify();
        }
    }

    /// Get one unused `conv`
    #[doc(hidden)]
    pub fn get_free_conv(&mut self) -> u32 {
        let mut inner = self.inner.borrow_mut();

        loop {
            let (c, _) = inner.alloc_conv.overflowing_add(1);
            inner.alloc_conv = c;
            if inner.alloc_conv == 0 {
                inner.alloc_conv = 1;
            }

            let conv = inner.alloc_conv;

            if !inner.sessions.contains_key(&conv) {
                break conv;
            }
        }
    }

    /// Stop updater and exit
    pub fn stop(&mut self) {
        let mut inner = self.inner.borrow_mut();
        inner.is_stop = true;
    }
}

impl<S> Stream for KcpSessionUpdater<S>
where
    S: Session,
{
    type Item = ();
    type Error = io::Error;
    fn poll(&mut self) -> Poll<Option<()>, io::Error> {
        let mut inner = self.inner.borrow_mut();
        if inner.is_stop {
            return Ok(Async::Ready(None));
        }

        try_ready!(inner.timeout.poll());

        let mut finished = Vec::new();
        let mut newly_push = Vec::new();
        while let Some((conv, InstantOrd(inst))) = inner.conv_queue.peek().map(|(c, i)| (*c, *i)) {
            let now = Instant::now();
            if inst > now {
                break;
            }

            let _ = inner.conv_queue.pop();

            let sess = inner.sessions
                            .get_mut(&conv)
                            .expect("Impossible! Cannot find session by conv!");
            match sess.poll() {
                Ok(Async::NotReady) => {
                    unreachable!();
                }
                Ok(Async::Ready(Some(next))) => {
                    newly_push.push((conv, InstantOrd(next)));
                }
                Ok(Async::Ready(None)) => {
                    finished.push(conv);
                }
                Err(err) => {
                    error!("[SESS] Update conv={} err: {:?}", conv, err);
                    finished.push(conv);
                }
            }
        }

        for conv in finished {
            if let Some(sess) = inner.sessions.remove(&conv) {
                let addr = sess.addr();
                let mut should_remove = false;
                if let Some(x) = inner.known_endpoint.get_mut(&addr) {
                    x.remove(&conv);

                    should_remove = x.is_empty();
                }

                if should_remove {
                    inner.known_endpoint.remove(&addr);
                }
            }
        }

        for (conv, inst) in newly_push {
            inner.conv_queue.push(conv, inst);
        }

        if let Some((_, &InstantOrd(inst))) = inner.conv_queue.peek() {
            inner.timeout.reset(inst);
            Ok(Async::Ready(Some(())))
        } else {
            inner.task = Some(task::current());
            Ok(Async::NotReady)
        }
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
    addr: SocketAddr,
    expire_dur: Duration,
    mode: KcpSessionMode,
}

impl KcpSession {
    pub fn new(kcp: SharedKcp, addr: SocketAddr, expire_dur: Duration, mode: KcpSessionMode) -> io::Result<KcpSession> {
        let mut n = KcpSession {
            kcp: kcp,
            addr: addr,
            expire_dur: expire_dur,
            mode: mode,
        };
        n.update()?;
        Ok(n)
    }

    pub fn new_shared(kcp: SharedKcp,
                      addr: SocketAddr,
                      expire_dur: Duration,
                      mode: KcpSessionMode)
                      -> io::Result<SharedKcpSession> {
        let sess = KcpSession::new(kcp, addr, expire_dur, mode)?;
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
    pub fn update(&mut self) -> io::Result<Instant> {
        self.kcp.update().map_err(From::from)
    }

    /// Called if it is expired
    pub fn expire(&mut self) -> io::Result<()> {
        self.kcp.set_expired()?;
        trace!("[SESS] addr={} conv={} is expired", self.addr, self.kcp.conv());
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
    pub fn poll(&mut self) -> Poll<Option<Instant>, io::Error> {
        // Session is already expired, drop this session
        if self.is_expired() {
            self.expire()?;
            return Ok(Async::Ready(None));
        }

        // Update it
        let next = self.update()?;
        self.kcp.try_notify_writable();

        // Check if it is closed
        if self.is_closed() {
            if self.mode == KcpSessionMode::Client {
                // Take over the UDP's control
                // Because in client mode, when the Stream is closed, session is the only one to be responsible
                // for receving data from udp and input to kcp.
                let _ = self.kcp.fetch();
            }

            if self.can_close() {
                trace!("[SESS] addr={} conv={} closing", self.addr, self.kcp.conv());
                return Ok(Async::Ready(None));
            }
        }

        Ok(Async::Ready(Some(next)))
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
}

impl KcpServerSession {
    pub fn new(sess: SharedKcpSession, r: SetReadiness) -> KcpServerSession {
        KcpServerSession {
            session: sess,
            readiness: r,
        }
    }
}

impl Stream for KcpServerSession {
    type Item = Instant;
    type Error = io::Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut sess = self.session.borrow_mut();
        match sess.poll() {
            Err(err) => {
                // Session is closed
                trace!("[SESS] Close and remove addr={} conv={}, err: {}", sess.addr, sess.kcp.conv(), err);

                // Awake pending reads
                self.readiness.set_readiness(Ready::readable())?;
                Err(err)
            }
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(Some(x))) => Ok(Async::Ready(Some(x))),
            Ok(Async::Ready(None)) => {
                // Session is closed
                trace!("[SESS] Close and remove addr={} conv={}", sess.addr, sess.kcp.conv());

                // Awake pending reads
                self.readiness.set_readiness(Ready::readable())?;
                Ok(Async::Ready(None))
            }
        }
    }
}

impl Session for KcpServerSession {
    /// Calls when you got data from transmission
    fn input(&mut self, buf: &[u8]) -> io::Result<()> {
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

    fn addr(&self) -> SocketAddr {
        let sess = self.session.borrow();
        *sess.addr()
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
    type Item = Instant;
    type Error = io::Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut sess = self.session.borrow_mut();
        sess.poll()
    }
}

impl Session for KcpClientSession {
    fn input(&mut self, buf: &[u8]) -> io::Result<()> {
        let mut sess = self.session.borrow_mut();
        sess.input(buf)
    }

    fn addr(&self) -> SocketAddr {
        let sess = self.session.borrow();
        *sess.addr()
    }
}
