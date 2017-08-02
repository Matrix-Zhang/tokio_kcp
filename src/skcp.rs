use std::cell::{self, RefCell};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::rc::Rc;

use kcp::Kcp;
use tokio_core::net::UdpSocket;

pub struct KcpOutput {
    udp: Rc<UdpSocket>,
    peer: SocketAddr,
}

impl KcpOutput {
    pub fn new(udp: Rc<UdpSocket>, peer: SocketAddr) -> KcpOutput {
        KcpOutput {
            udp: udp,
            peer: peer,
        }
    }
}

impl Write for KcpOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        trace!("[SEND] UDP {} size={} {:?}", self.peer, buf.len(), buf);
        self.udp.send_to(buf, &self.peer)
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
}
