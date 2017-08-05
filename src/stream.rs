use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::rc::Rc;
use std::time::Duration;

use bytes::BytesMut;
use futures::{Async, Future, Poll};
use kcp::Kcp;
use rand;
use tokio_core::net::UdpSocket;
use tokio_core::reactor::{Handle, PollEvented};
use tokio_io::{AsyncRead, AsyncWrite};

use config::KcpConfig;
use kcp_io::KcpIo;
use skcp::{KcpOutput, SharedKcp};

/// KCP client for interacting with server
pub struct KcpClientStream {
    udp: Rc<UdpSocket>,
    io: PollEvented<KcpIo>,
    buf: BytesMut,
}

impl Read for KcpClientStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.udp.recv_from(&mut self.buf) {
            Ok((n, addr)) => {
                trace!("[RECV] UDP {} size={} {:?}", addr, n, ::debug::BsDebug(&self.buf[..n]));
                self.io.get_mut().input_buf(&self.buf[..n])?;
            }
            Err(ref err) if err.kind() == ErrorKind::WouldBlock => {}
            Err(err) => return Err(err),
        }

        let n = self.io.read(buf)?;
        trace!("[RECV] Evented.read size={} {:?}", n, ::debug::BsDebug(&buf[..n]));
        Ok(n)
    }
}

impl AsyncRead for KcpClientStream {}

impl AsyncWrite for KcpClientStream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(().into())
    }
}

impl Write for KcpClientStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.get_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

/// Future for creating a new `KcpClientStream`
pub struct KcpStreamNew {
    addr: SocketAddr,
    handle: Handle,
    config: KcpConfig,
}

impl Future for KcpStreamNew {
    type Item = KcpClientStream;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<KcpClientStream, io::Error> {
        let local = SocketAddr::new(IpAddr::from(Ipv4Addr::new(0, 0, 0, 0)), 0);

        let udp = UdpSocket::bind(&local, &self.handle)?;
        let udp = Rc::new(udp);

        let mut kcp = Kcp::new(rand::random::<u32>(), KcpOutput::new(udp.clone(), self.addr, &self.handle));
        self.config.apply_config(&mut kcp);
        let shared_kcp = SharedKcp::new(kcp);

        let sess_exp = match self.config.session_expire {
            Some(dur) => dur,
            None => Duration::from_secs(90),
        };

        let mtu = self.config.mtu.unwrap_or(1500);
        let mut buf = BytesMut::with_capacity(mtu);
        unsafe {
            buf.set_len(mtu);
        }

        let io = KcpIo::new(shared_kcp, self.addr, &self.handle, None, sess_exp)?;
        let io = PollEvented::new(io, &self.handle)?;
        let stream = KcpClientStream {
            udp: udp,
            io: io,
            buf: buf,
        };
        Ok(Async::Ready(stream))
    }
}

/// KCP client between a local and remote socket
///
/// After creating a `KcpStream` by either connecting to a remote host or accepting a connection on a `KcpListener`,
/// data can be transmitted by reading and writing to it.
pub struct KcpStream {
    io: PollEvented<KcpIo>,
}

impl KcpStream {
    #[doc(hidden)]
    pub fn new(io: PollEvented<KcpIo>) -> KcpStream {
        KcpStream { io: io }
    }

    /// Opens a KCP connection to a remote host.
    pub fn connect(addr: &SocketAddr, handle: &Handle) -> KcpStreamNew {
        KcpStream::connect_with_config(addr, handle, KcpConfig::default())
    }

    /// Opens a KCP connection to a remote host.
    pub fn connect_with_config(addr: &SocketAddr, handle: &Handle, config: KcpConfig) -> KcpStreamNew {
        KcpStreamNew {
            addr: *addr,
            handle: handle.clone(),
            config: config,
        }
    }

    #[doc(hidden)]
    pub fn input_buf(&mut self, buf: &[u8]) -> io::Result<()> {
        let io = self.io.get_mut();
        io.input_buf(buf)
    }
}

impl Read for KcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io.read(buf)
    }
}

impl Write for KcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.get_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io.get_mut().flush()
    }
}

impl AsyncRead for KcpStream {}

impl AsyncWrite for KcpStream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(().into())
    }
}
