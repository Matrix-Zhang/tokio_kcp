use std::io;
use std::net::SocketAddr;
use std::rc::Rc;

use futures::{Async, Poll, Stream};
use kcp::{Kcp, get_conv};
use tokio_core::net::UdpSocket;
use tokio_core::reactor::{Handle, PollEvented};

use kcp_io::KcpIo;
use session::KcpSessionUpdater;
use skcp::{KcpOutput, SharedKcp};
use stream::KcpStream;

/// A KCP Socket server
pub struct KcpListener {
    udp: Rc<UdpSocket>,
    sessions: KcpSessionUpdater,
    handle: Handle,
}

/// An iterator that infinitely accepts connections on a `KcpListener`
pub struct Incoming {
    inner: KcpListener,
}

impl Stream for Incoming {
    type Item = (KcpStream, SocketAddr);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, io::Error> {
        Ok(Async::Ready(Some(try_nb!(self.inner.accept()))))
    }
}

impl KcpListener {
    /// Creates a new `KcpListener` which will be bound to the specific address.
    ///
    /// The returned listener is ready for accepting connections.
    pub fn bind(addr: &SocketAddr, handle: &Handle) -> io::Result<KcpListener> {
        UdpSocket::bind(addr, handle).map(|udp| {
                                              KcpListener {
                                                  udp: Rc::new(udp),
                                                  sessions: KcpSessionUpdater::new(),
                                                  handle: handle.clone(),
                                              }
                                          })
    }

    /// Returns the local socket address of this listener.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }

    /// Accept a new incoming connection from this listener.
    pub fn accept(&mut self) -> io::Result<(KcpStream, SocketAddr)> {
        let mut buf = [0; 1500];

        loop {
            let (size, addr) = self.udp.recv_from(&mut buf)?;

            if self.sessions.input_by_addr(&addr, &mut buf[..size])? {
                continue;
            }

            let kcp = Kcp::new(get_conv(&buf), KcpOutput::new(self.udp.clone(), addr));
            let shared_kcp = SharedKcp::new(kcp);

            let io = KcpIo::new(shared_kcp, addr, &self.handle, Some(self.sessions.clone()))?;
            let io = PollEvented::new(io, &self.handle)?;

            let mut stream = KcpStream::new(io);
            stream.input_buf(&buf[..size])?;

            return Ok((stream, addr));
        }
    }

    /// Returns an iterator over the connections being received on this listener.
    pub fn incoming(self) -> Incoming {
        Incoming { inner: self }
    }
}
