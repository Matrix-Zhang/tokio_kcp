use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::rc::Rc;
use std::time::Duration;

use futures::{Async, Poll};
use tokio_core::net::UdpSocket;
use tokio_core::reactor::{Handle, PollEvented};
use tokio_io::{AsyncRead, AsyncWrite};

use config::KcpConfig;
use kcp::get_conv;
use kcp_io::{ClientKcpIo, ServerKcpIo};
use session::{KcpClientSessionUpdater, KcpServerSessionUpdater};
use skcp::{KcpOutput, KcpOutputHandle, SharedKcp};

/// Default session expired timeout
const SESSION_EXPIRED_SECONDS: u64 = 90;

/// KCP client for interacting with server
pub struct KcpStream {
    udp: Rc<UdpSocket>,
    io: ClientKcpIo,
    buf: Vec<u8>,
}

impl KcpStream {
    #[doc(hidden)]
    pub fn new(udp: Rc<UdpSocket>, io: ClientKcpIo) -> KcpStream {
        let buf = vec![0u8; io.mtu()];
        KcpStream {
            udp: udp,
            io: io,
            buf: buf,
        }
    }

    /// Opens a KCP connection to a remote host.
    ///
    /// `conv` represents a conversation. Set to 0 will allow server to allocate one for you.
    pub fn connect(conv: u32,
                   addr: &SocketAddr,
                   handle: &Handle,
                   u: &mut KcpClientSessionUpdater)
                   -> io::Result<KcpStream> {
        KcpStream::connect_with_config(conv, addr, handle, u, &KcpConfig::default())
    }

    /// Opens a KCP connection to a remote host.
    ///
    /// `conv` represents a conversation. Set to 0 will allow server to allocate one for you.
    pub fn connect_with_config(conv: u32,
                               addr: &SocketAddr,
                               handle: &Handle,
                               u: &mut KcpClientSessionUpdater,
                               config: &KcpConfig)
                               -> io::Result<KcpStream> {
        let local = SocketAddr::new(IpAddr::from(Ipv4Addr::new(0, 0, 0, 0)), 0);

        let udp = UdpSocket::bind(&local, handle)?;
        let udp = Rc::new(udp);

        // Create a standalone output kcp
        let kcp = SharedKcp::new(config, conv, udp.clone(), *addr, handle, config.stream);

        let sess_exp = match config.session_expire {
            Some(dur) => dur,
            None => Duration::from_secs(SESSION_EXPIRED_SECONDS),
        };

        let local_addr = udp.local_addr().expect("Failed to get local addr");
        let io = ClientKcpIo::new(kcp, local_addr, sess_exp, u)?;
        Ok(KcpStream::new(udp, io))
    }

    fn recv_from(&mut self) -> io::Result<()> {
        match self.udp.recv_from(&mut self.buf) {
            Ok((n, addr)) => {
                let buf = &self.buf[..n];

                trace!("[RECV] UDP addr={} conv={} size={} {:?}", addr, get_conv(buf), n, ::debug::BsDebug(buf));
                match self.io.input(buf) {
                    Ok(..) => Ok(()),
                    Err(err) => {
                        error!("[RECV] Input for local addr={} error, recv addr={}, error: {:?}",
                               self.udp.local_addr().unwrap(),
                               addr,
                               err);
                        Err(err)
                    }
                }
            }
            Err(err) => Err(err),
        }
    }

    fn io_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.io.read(buf) {
            Ok(n) => {
                trace!("[RECV] Evented.read size={}", n);
                Ok(n)
            }
            Err(err) => Err(err),
        }
    }
}

impl Read for KcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // loop until we got something
        loop {
            match self.io_read(buf) {
                Ok(n) => return Ok(n),
                // Loop continue, maybe we have received an ACK packet
                Err(ref err) if err.kind() == ErrorKind::WouldBlock => {}
                Err(err) => return Err(err),
            }

            self.recv_from()?;
        }
    }
}

impl AsyncRead for KcpStream {}

impl AsyncWrite for KcpStream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(().into())
    }
}

impl Write for KcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

/// KCP client between a local and remote socket
///
/// After creating a `KcpStream` by either connecting to a remote host or accepting a connection on a `KcpListener`,
/// data can be transmitted by reading and writing to it.
pub struct ServerKcpStream {
    io: PollEvented<ServerKcpIo>,
}

impl ServerKcpStream {
    #[doc(hidden)]
    pub fn new_with_config(conv: u32,
                           output_handle: KcpOutputHandle,
                           addr: &SocketAddr,
                           handle: &Handle,
                           u: &mut KcpServerSessionUpdater,
                           config: &KcpConfig)
                           -> io::Result<ServerKcpStream> {
        let output = KcpOutput::new_with_handle(output_handle, *addr);
        let kcp = SharedKcp::new_with_output(config, conv, output, config.stream);

        let sess_exp = match config.session_expire {
            Some(dur) => dur,
            None => Duration::from_secs(SESSION_EXPIRED_SECONDS),
        };

        let io = ServerKcpIo::new(kcp, *addr, sess_exp, u)?;
        let io = PollEvented::new(io, handle)?;
        Ok(ServerKcpStream { io: io })

    }

    #[doc(hidden)]
    pub fn input(&mut self, buf: &[u8]) -> io::Result<()> {
        let io = self.io.get_mut();
        io.input(buf)
    }
}

impl Read for ServerKcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io.read(buf)
    }
}

impl Write for ServerKcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // FIXME: Write does not have events yet
        self.io.get_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        // FIXME: Write does not have events yet
        self.io.get_mut().flush()
    }
}

impl AsyncRead for ServerKcpStream {}

impl AsyncWrite for ServerKcpStream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.io.get_mut().shutdown()?;
        Ok(Async::Ready(()))
    }
}
