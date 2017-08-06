use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, ErrorKind, Write};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use bytes::buf::FromBuf;
use futures::{Async, Future, Poll};
use futures::task::{self, Task};
use kcp::{Kcp, KcpResult};
use tokio_core::net::UdpSocket;
use tokio_core::reactor::Handle;

use config::KcpConfig;

struct KcpOutputInner {
    udp: Rc<UdpSocket>,
    task: Option<Task>,
    pkt_queue: VecDeque<(SocketAddr, Bytes)>,
    is_finished: bool,
}

impl KcpOutputInner {
    fn new(udp: Rc<UdpSocket>) -> KcpOutputInner {
        KcpOutputInner {
            udp: udp,
            task: None,
            pkt_queue: VecDeque::new(),
            is_finished: false,
        }
    }

    fn notify(&mut self) {
        if let Some(task) = self.task.take() {
            task.notify();
        }
    }

    fn push_packet(&mut self, pkt: Bytes, peer: SocketAddr) {
        self.pkt_queue.push_back((peer, pkt));
        self.notify();
    }

    fn close(&mut self) {
        self.is_finished = true;
        self.notify();
    }

    fn is_empty(&self) -> bool {
        self.pkt_queue.is_empty()
    }

    fn send_or_push(&mut self, buf: &[u8], peer: &SocketAddr) -> io::Result<usize> {
        if self.is_empty() {
            match self.udp.send_to(buf, peer) {
                Ok(n) => {
                    trace!("[SEND] Immediately UDP {} size={} {:?}", peer, buf.len(), ::debug::BsDebug(buf));
                    if n != buf.len() {
                        error!("[SEND] Immediately Sent size={}, but packet is size={}", n, buf.len());
                    }
                    return Ok(n);
                }
                Err(ref err) if err.kind() == ErrorKind::WouldBlock => {}
                Err(err) => return Err(err),
            }
        }

        trace!("[SEND] Delay UDP {} size={} qsize={} {:?}",
               peer,
               buf.len(),
               self.pkt_queue.len(),
               ::debug::BsDebug(buf));

        self.push_packet(Bytes::from_buf(buf), *peer);
        Ok(buf.len())
    }
}

impl Drop for KcpOutputInner {
    fn drop(&mut self) {
        self.close();
    }
}

struct KcpOutputQueue {
    inner: Rc<RefCell<KcpOutputInner>>,
}

impl Future for KcpOutputQueue {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        let mut inner = self.inner.borrow_mut();

        while !inner.pkt_queue.is_empty() {
            {
                let &(ref peer, ref pkt) = &inner.pkt_queue[0];
                let n = try_nb!(inner.udp.send_to(&pkt, &peer));
                trace!("[SEND] Delayed UDP {} size={} {:?}", peer, pkt.len(), pkt);
                if n != pkt.len() {
                    error!("[SEND] Delayed Sent size={}, but packet is size={}", n, pkt.len());
                }
            }

            let _ = inner.pkt_queue.pop_front();
        }

        if inner.is_finished {
            Ok(Async::Ready(()))
        } else {
            inner.task = Some(task::current());
            Ok(Async::NotReady)
        }
    }
}

#[derive(Clone)]
pub struct KcpOutputHandle {
    inner: Rc<RefCell<KcpOutputInner>>,
}

impl KcpOutputHandle {
    pub fn new(udp: Rc<UdpSocket>, handle: &Handle) -> KcpOutputHandle {
        let inner = KcpOutputInner::new(udp);
        let inner = Rc::new(RefCell::new(inner));
        let queue = KcpOutputQueue { inner: inner.clone() };
        handle.spawn(queue.map_err(move |err| {
                                       error!("[SEND] UDP output queue failed, err: {:?}", err);
                                   }));
        KcpOutputHandle { inner: inner }
    }

    fn send_to(&mut self, buf: &[u8], peer: &SocketAddr) -> io::Result<usize> {
        let mut inner = self.inner.borrow_mut();
        inner.send_or_push(buf, peer)
    }
}

pub struct KcpOutput {
    inner: KcpOutputHandle,
    peer: SocketAddr,
}

impl KcpOutput {
    pub fn new(udp: Rc<UdpSocket>, peer: SocketAddr, handle: &Handle) -> KcpOutput {
        KcpOutput::new_with_handle(KcpOutputHandle::new(udp, handle), peer)
    }

    pub fn new_with_handle(h: KcpOutputHandle, peer: SocketAddr) -> KcpOutput {
        KcpOutput {
            inner: h,
            peer: peer,
        }
    }
}

impl Write for KcpOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.send_to(buf, &self.peer)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct KcpCell {
    kcp: Kcp<KcpOutput>,
    last_update: Instant,
    is_closed: bool,
}

#[derive(Clone)]
pub struct SharedKcp {
    inner: Rc<RefCell<KcpCell>>,
}

impl SharedKcp {
    pub fn new(c: &KcpConfig, conv: u32, udp: Rc<UdpSocket>, peer: SocketAddr, handle: &Handle) -> SharedKcp {
        let output = KcpOutput::new(udp, peer, handle);
        SharedKcp::new_with_output(c, conv, output)
    }

    pub fn new_with_output(c: &KcpConfig, conv: u32, output: KcpOutput) -> SharedKcp {
        let mut kcp = Kcp::new(conv, output);
        c.apply_config(&mut kcp);

        SharedKcp {
            inner: Rc::new(RefCell::new(KcpCell {
                                            kcp: kcp,
                                            last_update: Instant::now(),
                                            is_closed: false,
                                        })),
        }
    }

    /// Call every time you got data from transmission
    pub fn input(&mut self, buf: &[u8]) -> KcpResult<()> {
        let mut inner = self.inner.borrow_mut();
        inner.kcp.input(buf)?;
        inner.last_update = Instant::now();
        Ok(())
    }

    /// Call if you want to send some data
    pub fn send(&mut self, buf: &[u8]) -> KcpResult<usize> {
        let mut inner = self.inner.borrow_mut();
        let n = inner.kcp.send(buf)?;
        inner.last_update = Instant::now();
        Ok(n)
    }

    /// Call if you want to get some data
    /// Always call right after input
    pub fn recv(&mut self, buf: &mut [u8]) -> KcpResult<usize> {
        let mut inner = self.inner.borrow_mut();
        let n = inner.kcp.recv(buf)?;
        inner.last_update = Instant::now();
        Ok(n)
    }

    /// Call if you want to flush all pending data in queue
    pub fn flush(&mut self) -> KcpResult<()> {
        let mut inner = self.inner.borrow_mut();
        inner.kcp.flush()?;
        inner.last_update = Instant::now();
        Ok(())
    }

    /// Tell me how long this session have no interaction
    pub fn elapsed(&self) -> Duration {
        let inner = self.inner.borrow();
        inner.last_update.elapsed()
    }

    /// Make this session expire, all read apis will return 0 (EOF)
    /// It will flush the buffer when it is called
    pub fn set_expired(&mut self) -> KcpResult<()> {
        let mut inner = self.inner.borrow_mut();
        inner.kcp.expired();
        inner.kcp.flush()
    }

    /// Call in every tick
    /// Returns when to call this function again
    pub fn update(&mut self) -> KcpResult<Instant> {
        let mut inner = self.inner.borrow_mut();
        inner.kcp.update(::current())?;
        let next = inner.kcp.check(::current());
        Ok(Instant::now() + Duration::from_millis(next as u64))
    }

    /// Check if send queue is empty
    pub fn has_waitsnd(&self) -> bool {
        let inner = self.inner.borrow();
        inner.kcp.waitsnd() != 0
    }

    /// Get mtu
    pub fn mtu(&self) -> usize {
        let inner = self.inner.borrow();
        inner.kcp.mtu()
    }

    /// Set is close
    pub fn close(&mut self) {
        let mut inner = self.inner.borrow_mut();
        inner.is_closed = true;
    }

    /// Check if it is closed
    pub fn is_closed(&self) -> bool {
        let inner = self.inner.borrow();
        inner.is_closed
    }

    /// Check if it can read
    pub fn can_read(&self) -> bool {
        let inner = self.inner.borrow();
        inner.kcp.peeksize().unwrap_or(0) != 0
    }

    /// Peek
    pub fn peeksize(&self) -> usize {
        let inner = self.inner.borrow();
        inner.kcp.peeksize().unwrap_or(0)
    }
}
