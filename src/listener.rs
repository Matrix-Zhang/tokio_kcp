use std::io;
use std::net::SocketAddr;
use std::rc::Rc;

use bytes::BytesMut;
use futures::{Async, Poll, Stream};
use kcp::get_conv;
use tokio_core::net::UdpSocket;
use tokio_core::reactor::Handle;

use config::KcpConfig;
use session::KcpSessionUpdater;
use stream::ServerKcpStream;

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
    type Item = (ServerKcpStream, SocketAddr);
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
    pub fn accept(&mut self) -> io::Result<(ServerKcpStream, SocketAddr)> {
        let mtu = self.config.mtu.unwrap_or(1500);
        let mut buf = BytesMut::with_capacity(mtu);
        unsafe {
            buf.set_len(mtu);
        }

        loop {
            let (size, addr) = self.udp.recv_from(&mut buf)?;

            if self.sessions.input_by_addr(&addr, &mut buf[..size])? {
                trace!("[RECV] size={} addr={} {:?}", size, addr, ::debug::BsDebug(&buf[..size]));
                continue;
            }

            trace!("[ACPT] Accepted connection {}", addr);

            let mut stream = ServerKcpStream::new_with_config(get_conv(&buf),
                                                              self.udp.clone(),
                                                              &addr,
                                                              &self.handle,
                                                              &mut self.sessions,
                                                              &self.config)?;

            // Input the initial packet
            stream.input(&buf[..size])?;

            return Ok((stream, addr));
        }
    }

    /// Returns an iterator over the connections being received on this listener.
    pub fn incoming(self) -> Incoming {
        Incoming { inner: self }
    }
}
