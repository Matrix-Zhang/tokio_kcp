use std::cell::{self, RefCell};
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures::{Async, Future, Poll, Stream};
use mio::{Ready, SetReadiness};
use tokio_core::reactor::Timeout;

use kcp_io::KcpIoMode;
use skcp::SharedKcp;

#[derive(Clone)]
pub struct KcpSessionUpdater {
    sessions: Rc<RefCell<HashMap<SocketAddr, SharedKcpSession>>>,
}

impl KcpSessionUpdater {
    pub fn new() -> KcpSessionUpdater {
        KcpSessionUpdater { sessions: Rc::new(RefCell::new(HashMap::new())) }
    }
}

impl KcpSessionUpdater {
    fn sessions_mut(&mut self) -> cell::RefMut<HashMap<SocketAddr, SharedKcpSession>> {
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

    pub fn insert_by_addr(&mut self, addr: SocketAddr, s: SharedKcpSession) {
        let mut ses = self.sessions_mut();
        ses.insert(addr, s);
    }
}

pub struct KcpSession {
    kcp: SharedKcp,
    timer: Timeout,
    last_update: Rc<RefCell<Instant>>,
    readiness: SetReadiness,
    owner: Option<KcpSessionUpdater>,
    addr: SocketAddr,
    close_flag: Rc<RefCell<bool>>,
    mode: KcpIoMode,
}

impl KcpSession {
    fn set_last_update(&mut self, t: Instant) {
        let mut u = self.last_update.borrow_mut();
        *u = t;
    }
}

impl KcpSession {
    fn try_remove_self(&mut self) {
        if let Some(ref mut u) = self.owner {
            u.remove_by_addr(&self.addr);
        }
    }
}

#[derive(Clone)]
pub struct SharedKcpSession {
    inner: Rc<RefCell<KcpSession>>,
}

impl SharedKcpSession {
    pub fn new(kcp: SharedKcp,
               timer: Timeout,
               last_update: Rc<RefCell<Instant>>,
               readiness: SetReadiness,
               addr: SocketAddr,
               mut owner: Option<KcpSessionUpdater>,
               close_flag: Rc<RefCell<bool>>,
               mode: KcpIoMode)
               -> io::Result<SharedKcpSession> {
        let inner = KcpSession {
            kcp: kcp,
            timer: timer,
            last_update: last_update,
            readiness: readiness,
            owner: owner.clone(),
            addr: addr,
            close_flag: close_flag,
            mode: mode,
        };

        let mut session = SharedKcpSession { inner: Rc::new(RefCell::new(inner)) };

        if let Some(ref mut sm) = owner {
            sm.insert_by_addr(addr, session.clone());
        }

        session.update_kcp()?;
        Ok(session)
    }

    fn borrow<'a>(&'a self) -> cell::Ref<'a, KcpSession> {
        self.inner.borrow()
    }

    fn borrow_mut<'a>(&'a mut self) -> cell::RefMut<'a, KcpSession> {
        self.inner.borrow_mut()
    }

    fn poll_timer(&mut self) -> Poll<(), io::Error> {
        let mut inner = self.borrow_mut();
        inner.timer.poll()
    }

    fn elapsed(&self) -> Duration {
        let inner = self.borrow();
        let last_update = inner.last_update.borrow();
        last_update.elapsed()
    }

    pub fn input(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let mut inner = self.borrow_mut();
        inner.set_last_update(Instant::now());

        {
            let mut kcp = inner.kcp.borrow_mut();
            kcp.input(buf)?;
        }

        inner.readiness.set_readiness(Ready::readable())?;

        Ok(())
    }

    #[inline]
    fn is_expired(&self) -> bool {
        let dur = {
            let inner = self.borrow();
            match inner.mode {
                KcpIoMode::Client => return false,
                KcpIoMode::Server(dur) => dur,
            }
        };

        self.elapsed() >= dur
    }

    fn update_kcp(&mut self) -> io::Result<()> {
        let mut inner = self.borrow_mut();

        let curr = ::current();
        let now = Instant::now();

        let next_dur = {
            let mut kcp = inner.kcp.borrow_mut();
            kcp.update(curr)?;
            Duration::from_millis(kcp.check(curr) as u64)
        };

        let update = now + next_dur;
        inner.timer.reset(update);
        Ok(())
    }

    fn expire_kcp(&mut self) -> io::Result<()> {
        let mut inner = self.borrow_mut();
        {
            let mut kcp = inner.kcp.borrow_mut();
            kcp.expired();
        }
        inner.readiness.set_readiness(Ready::readable())?;
        inner.try_remove_self();

        Ok(())
    }

    fn is_closed(&self) -> bool {
        let inner = self.borrow();
        let cf = inner.close_flag.borrow();
        *cf
    }
}

impl Stream for SharedKcpSession {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<()>, io::Error> {
        if self.is_closed() {
            trace!("[SESS] KcpSession {} closed", self.borrow().addr);
            return Ok(Async::Ready(None));
        }

        let _ = try_ready!(self.poll_timer());

        if !self.is_expired() {
            self.update_kcp()?;
            Ok(Async::Ready(Some(())))
        } else {
            trace!("[SESS] KcpSession {} expired", self.borrow().addr);
            self.expire_kcp()?;
            Ok(Async::Ready(None))
        }
    }
}
