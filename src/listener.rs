use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use futures::{Async, Future, Poll, Stream};
use kcp::{get_conv, set_conv};
use tokio_core::net::UdpSocket;
use tokio_core::reactor::Handle;

use config::KcpConfig;
use session::KcpSessionUpdater;
use skcp::KcpOutputHandle;
use stream::ServerKcpStream;

/// A KCP Socket server
pub struct KcpListener {
    udp: Rc<UdpSocket>,
    sessions: KcpSessionUpdater,
    handle: Handle,
    config: KcpConfig,
    buf: Vec<u8>,
    output_handle: KcpOutputHandle,
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
        let updater = KcpSessionUpdater::new(Duration::from_millis(5), handle)?;

        let run_updater = updater.clone();
        handle.spawn(run_updater.for_each(|_| Ok(())).map_err(|err| {
                                                                  error!("Failed to update sessions, err: {:?}", err);
                                                              }));

        UdpSocket::bind(addr, handle).map(|udp| {
            let shared_udp = Rc::new(udp);
            let output_handle = KcpOutputHandle::new(shared_udp.clone(), handle);

            KcpListener {
                udp: shared_udp,
                sessions: updater,
                handle: handle.clone(),
                config: config,
                buf: vec![0u8; config.mtu.unwrap_or(1400)],
                output_handle: output_handle,
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
        loop {
            let (size, addr) = self.udp.recv_from(&mut self.buf)?;

            let mut buf = &mut self.buf[..size];
            let mut conv = get_conv(&*buf);
            trace!("[RECV] size={} conv={} addr={} {:?}", size, conv, addr, ::debug::BsDebug(buf));

            if self.sessions.input_by_conv(conv, buf)? {
                continue;
            }

            trace!("[ACPT] Accepted connection {}", addr);

            // Set `conv` to 0 means let the server allocate a `conv` for it
            if conv == 0 {
                conv = self.sessions.get_free_conv();
                trace!("[ACPT] Allocated conv={} for {}", conv, addr);

                // Set to buffer
                set_conv(buf, conv);
            }

            let mut stream = ServerKcpStream::new_with_config(conv,
                                                              self.output_handle.clone(),
                                                              &addr,
                                                              &self.handle,
                                                              &mut self.sessions,
                                                              &self.config)?;

            // Input the initial packet
            stream.input(&*buf)?;

            return Ok((stream, addr));
        }
    }

    /// Returns an iterator over the connections being received on this listener.
    pub fn incoming(self) -> Incoming {
        Incoming { inner: self }
    }
}
