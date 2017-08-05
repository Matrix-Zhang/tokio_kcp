use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use bytes::BytesMut;
use futures::{Async, Poll, Stream};
use kcp::{Kcp, get_conv};
use tokio_core::net::UdpSocket;
use tokio_core::reactor::{Handle, PollEvented};

use config::KcpConfig;
use kcp_io::KcpIo;
use session::KcpSessionUpdater;
use skcp::{KcpOutput, SharedKcp};
use stream::KcpStream;

/// A KCP Socket server
pub struct KcpListener {
    udp: Rc<UdpSocket>,
    sessions: KcpSessionUpdater,
    handle: Handle,
    config: KcpConfig,
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
    pub fn bind_with_config(addr: &SocketAddr, handle: &Handle, config: KcpConfig) -> io::Result<KcpListener> {
        UdpSocket::bind(addr, handle).map(|udp| {
                                              KcpListener {
                                                  udp: Rc::new(udp),
                                                  sessions: KcpSessionUpdater::new(),
                                                  handle: handle.clone(),
                                                  config: config,
                                              }
                                          })
    }

    /// Creates a new `KcpListener` which will be bound to the specific address with default config.
    ///
    /// The returned listener is ready for accepting connections.
    pub fn bind(addr: &SocketAddr, handle: &Handle) -> io::Result<KcpListener> {
        KcpListener::bind_with_config(addr, handle, KcpConfig::default())
    }

    /// Returns the local socket address of this listener.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }

    /// Accept a new incoming connection from this listener.
    pub fn accept(&mut self) -> io::Result<(KcpStream, SocketAddr)> {
        let mtu = self.config.mtu.unwrap_or(1500);
        let mut buf = BytesMut::with_capacity(mtu);
        unsafe {
            buf.set_len(mtu);
        }

        loop {
            let (size, addr) = self.udp.recv_from(&mut buf)?;

            if self.sessions.input_by_addr(&addr, &mut buf[..size])? {
                continue;
            }

            trace!("[ACPT] Accepted connection {}", addr);

            let mut kcp = Kcp::new(get_conv(&buf), KcpOutput::new(self.udp.clone(), addr, &self.handle));
            self.config.apply_config(&mut kcp);
            let shared_kcp = SharedKcp::new(kcp);

            let sess_exp = match self.config.session_expire {
                Some(dur) => dur,
                None => Duration::from_secs(90),
            };

            let io = KcpIo::new(shared_kcp, addr, &self.handle, Some(self.sessions.clone()), sess_exp)?;
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
