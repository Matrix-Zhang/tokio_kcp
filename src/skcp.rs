use std::cell::{self, RefCell};
use std::collections::VecDeque;
use std::io::{self, ErrorKind, Write};
use std::net::SocketAddr;
use std::rc::Rc;

use bytes::Bytes;
use bytes::buf::FromBuf;
use futures::{Async, Future, Poll};
use futures::task::{self, Task};
use kcp::Kcp;
use tokio_core::net::UdpSocket;
use tokio_core::reactor::Handle;

struct KcpOutputInner {
    udp: Rc<UdpSocket>,
    peer: SocketAddr,
    task: Option<Task>,
    pkt_queue: VecDeque<Bytes>,
    is_finished: bool,
}

impl KcpOutputInner {
    fn new(udp: Rc<UdpSocket>, peer: SocketAddr) -> KcpOutputInner {
        KcpOutputInner {
            udp: udp,
            peer: peer,
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

    fn push_packet(&mut self, pkt: Bytes) {
        self.pkt_queue.push_back(pkt);
        self.notify();
    }

    fn close(&mut self) {
        self.is_finished = true;
        self.notify();
    }

    fn is_empty(&self) -> bool {
        self.pkt_queue.is_empty()
    }

    fn send_or_push(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.is_empty() {
            match self.udp.send_to(buf, &self.peer) {
                Ok(n) => {
                    trace!("[SEND] Immediately UDP {} size={} {:?}", self.peer, buf.len(), ::debug::BsDebug(buf));
                    if n != buf.len() {
                        error!("[SEND] Immediately Sent size={}, but packet is size={}", n, buf.len());
                    }
                    return Ok(n);
                }
                Err(ref err) if err.kind() == ErrorKind::WouldBlock => {}
                Err(err) => return Err(err),
            }
        }

        self.push_packet(Bytes::from_buf(buf));
        Ok(buf.len())
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
                let pkt = &inner.pkt_queue[0];
                let n = try_nb!(inner.udp.send_to(&pkt, &inner.peer));
                trace!("[SEND] Delayed UDP {} size={} {:?}", inner.peer, pkt.len(), pkt);
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

pub struct KcpOutput {
    inner: Rc<RefCell<KcpOutputInner>>,
}

impl Drop for KcpOutput {
    fn drop(&mut self) {
        let mut inner = self.inner.borrow_mut();
        inner.close();
    }
}

impl KcpOutput {
    pub fn new(udp: Rc<UdpSocket>, peer: SocketAddr, handle: &Handle) -> KcpOutput {
        let inner = KcpOutputInner::new(udp, peer);
        let inner = Rc::new(RefCell::new(inner));

        let queue = KcpOutputQueue { inner: inner.clone() };
        handle.spawn(queue.map_err(move |err| {
                                       error!("[SEND] UDP output failed, peer: {}, err: {:?}", peer, err);
                                   }));

        KcpOutput { inner: inner }
    }
}

impl Write for KcpOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = self.inner.borrow_mut();
        inner.send_or_push(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct SharedKcp {
    kcp: Rc<RefCell<Kcp<KcpOutput>>>,
}

impl SharedKcp {
    pub fn new(kcp: Kcp<KcpOutput>) -> SharedKcp {
        SharedKcp { kcp: Rc::new(RefCell::new(kcp)) }
    }

    pub fn borrow_mut<'a>(&'a mut self) -> cell::RefMut<'a, Kcp<KcpOutput>> {
        self.kcp.borrow_mut()
    }

    pub fn borrow<'a>(&'a self) -> cell::Ref<'a, Kcp<KcpOutput>> {
        self.kcp.borrow()
    }
}
