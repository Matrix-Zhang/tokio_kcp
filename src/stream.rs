use std::{
    io::{self, ErrorKind},
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures::{future, ready};
use kcp::{Error as KcpError, KcpResult};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::UdpSocket,
    sync::mpsc,
};

use crate::{config::KcpConfig, session::KcpSession, skcp::KcpSocket};

pub struct KcpStream {
    session: Arc<KcpSession>,
    udp: Arc<UdpSocket>,
    recv_buffer: Vec<u8>,
    recv_buffer_pos: usize,
    recv_buffer_cap: usize,
}

impl Drop for KcpStream {
    fn drop(&mut self) {
        self.session.close();
    }
}

impl KcpStream {
    pub async fn connect(config: &KcpConfig, addr: SocketAddr) -> KcpResult<KcpStream> {
        let udp = match addr.ip() {
            IpAddr::V4(..) => UdpSocket::bind("0.0.0.0:0").await?,
            IpAddr::V6(..) => UdpSocket::bind("[::]:0").await?,
        };

        let udp = Arc::new(udp);
        let socket = KcpSocket::new(config, 0, udp.clone(), addr, config.stream)?;

        let session = KcpSession::new_shared(socket, config.session_expire, None);

        Ok(KcpStream {
            session,
            udp,
            recv_buffer: Vec::new(),
            recv_buffer_pos: 0,
            recv_buffer_cap: 0,
        })
    }

    pub fn poll_send(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<KcpResult<usize>> {
        // Mutex doesn't have poll_lock, spinning on it.
        let socket = self.session.kcp_socket();
        let mut kcp = match socket.try_lock() {
            Ok(guard) => guard,
            Err(..) => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };

        kcp.poll_send(cx, buf)
    }

    pub async fn send(&mut self, buf: &[u8]) -> KcpResult<usize> {
        future::poll_fn(|cx| self.poll_send(cx, buf)).await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<KcpResult<usize>> {
        loop {
            // Consumes all data in buffer
            if self.recv_buffer_pos < self.recv_buffer_cap {
                let remaining = self.recv_buffer_cap - self.recv_buffer_pos;
                let copy_length = remaining.min(buf.len());

                buf.copy_from_slice(&self.recv_buffer[self.recv_buffer_pos..self.recv_buffer_pos + copy_length]);
                self.recv_buffer_pos += copy_length;
                return Ok(copy_length).into();
            }

            // Mutex doesn't have poll_lock, spinning on it.
            let socket = self.session.kcp_socket();
            let mut kcp = match socket.try_lock() {
                Ok(guard) => guard,
                Err(..) => {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
            };

            loop {
                // Try to read from KCP
                // 1. Read directly with user provided `buf`
                match kcp.try_recv(buf) {
                    Ok(n) => return Ok(n).into(),
                    Err(KcpError::RecvQueueEmpty) => {
                        // Nothing in recv queue, read from UDP socket
                        let mut packet_buffer = [0u8; 65536];
                        let mut read_buffer = ReadBuf::new(&mut packet_buffer);
                        ready!(self.udp.poll_recv(cx, &mut read_buffer))?;
                        let packet = read_buffer.filled();

                        log::trace!("client read {} bytes", packet.len());

                        kcp.input(packet)?;
                        continue;
                    }
                    Err(KcpError::UserBufTooSmall) => {}
                    Err(err) => return Err(err).into(),
                }

                // 2. User `buf` too small, read to recv_buffer
                let required_size = kcp.peek_size()?;
                if self.recv_buffer.len() < required_size {
                    self.recv_buffer.resize(required_size, 0);
                }

                match kcp.try_recv(&mut self.recv_buffer) {
                    Ok(n) => {
                        self.recv_buffer_pos = 0;
                        self.recv_buffer_cap = n;
                        break;
                    }
                    Err(err) => return Err(err).into(),
                }
            }
        }
    }

    pub async fn recv(&mut self, buf: &mut [u8]) -> KcpResult<usize> {
        future::poll_fn(|cx| self.poll_recv(cx, buf)).await
    }
}

impl AsyncRead for KcpStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match ready!(self.poll_recv(cx, buf.initialize_unfilled())) {
            Ok(n) => {
                buf.advance(n);
                Ok(()).into()
            }
            Err(KcpError::IoError(err)) => Err(err).into(),
            Err(err) => Err(io::Error::new(ErrorKind::Other, err)).into(),
        }
    }
}

impl AsyncWrite for KcpStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match ready!(self.poll_send(cx, buf)) {
            Ok(n) => Ok(n).into(),
            Err(KcpError::IoError(err)) => Err(err).into(),
            Err(err) => Err(io::Error::new(ErrorKind::Other, err)).into(),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Mutex doesn't have poll_lock, spinning on it.
        let socket = self.session.kcp_socket();
        let mut kcp = match socket.try_lock() {
            Ok(guard) => guard,
            Err(..) => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };

        match kcp.flush() {
            Ok(..) => Ok(()).into(),
            Err(KcpError::IoError(err)) => Err(err).into(),
            Err(err) => Err(io::Error::new(ErrorKind::Other, err)).into(),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Ok(()).into()
    }
}

pub struct KcpServerStream {
    session: Arc<KcpSession>,
    recv_buffer: Vec<u8>,
    recv_buffer_pos: usize,
    recv_buffer_cap: usize,
}

impl Drop for KcpServerStream {
    fn drop(&mut self) {
        self.session.close();
    }
}

impl KcpServerStream {
    pub fn new(
        config: &KcpConfig,
        udp: Arc<UdpSocket>,
        conv: u32,
        peer_addr: SocketAddr,
        session_close_notifier: mpsc::Sender<u32>,
    ) -> KcpResult<KcpServerStream> {
        let socket = KcpSocket::new(config, conv, udp, peer_addr, true)?;
        let session = KcpSession::new_shared(socket, config.session_expire, Some(session_close_notifier));
        Ok(KcpServerStream::with_session(session))
    }

    pub fn with_session(session: Arc<KcpSession>) -> KcpServerStream {
        KcpServerStream {
            session,
            recv_buffer: Vec::new(),
            recv_buffer_pos: 0,
            recv_buffer_cap: 0,
        }
    }

    pub fn poll_send(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<KcpResult<usize>> {
        // Mutex doesn't have poll_lock, spinning on it.
        let socket = self.session.kcp_socket();
        let mut kcp = match socket.try_lock() {
            Ok(guard) => guard,
            Err(..) => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };

        kcp.poll_send(cx, buf)
    }

    pub async fn send(&mut self, buf: &[u8]) -> KcpResult<usize> {
        future::poll_fn(|cx| self.poll_send(cx, buf)).await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<KcpResult<usize>> {
        loop {
            // Consumes all data in buffer
            if self.recv_buffer_pos < self.recv_buffer_cap {
                let remaining = self.recv_buffer_cap - self.recv_buffer_pos;
                let copy_length = remaining.min(buf.len());

                buf.copy_from_slice(&self.recv_buffer[self.recv_buffer_pos..self.recv_buffer_pos + copy_length]);
                self.recv_buffer_pos += copy_length;
                return Ok(copy_length).into();
            }

            // Mutex doesn't have poll_lock, spinning on it.
            let socket = self.session.kcp_socket();
            let mut kcp = match socket.try_lock() {
                Ok(guard) => guard,
                Err(..) => {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
            };

            // Try to read from KCP
            // 1. Read directly with user provided `buf`
            match ready!(kcp.poll_recv(cx, buf)) {
                Ok(n) => return Ok(n).into(),
                Err(KcpError::UserBufTooSmall) => {}
                Err(err) => return Err(err).into(),
            }

            // 2. User `buf` too small, read to recv_buffer
            let required_size = kcp.peek_size()?;
            if self.recv_buffer.len() < required_size {
                self.recv_buffer.resize(required_size, 0);
            }

            match ready!(kcp.poll_recv(cx, &mut self.recv_buffer)) {
                Ok(n) => {
                    self.recv_buffer_pos = 0;
                    self.recv_buffer_cap = n;
                }
                Err(err) => return Err(err).into(),
            }
        }
    }

    pub async fn recv(&mut self, buf: &mut [u8]) -> KcpResult<usize> {
        future::poll_fn(|cx| self.poll_recv(cx, buf)).await
    }
}

impl AsyncRead for KcpServerStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match ready!(self.poll_recv(cx, buf.initialize_unfilled())) {
            Ok(n) => {
                buf.advance(n);
                Ok(()).into()
            }
            Err(KcpError::IoError(err)) => Err(err).into(),
            Err(err) => Err(io::Error::new(ErrorKind::Other, err)).into(),
        }
    }
}

impl AsyncWrite for KcpServerStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match ready!(self.poll_send(cx, buf)) {
            Ok(n) => Ok(n).into(),
            Err(KcpError::IoError(err)) => Err(err).into(),
            Err(err) => Err(io::Error::new(ErrorKind::Other, err)).into(),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Mutex doesn't have poll_lock, spinning on it.
        let socket = self.session.kcp_socket();
        let mut kcp = match socket.try_lock() {
            Ok(guard) => guard,
            Err(..) => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };

        match kcp.flush() {
            Ok(..) => Ok(()).into(),
            Err(KcpError::IoError(err)) => Err(err).into(),
            Err(err) => Err(io::Error::new(ErrorKind::Other, err)).into(),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Ok(()).into()
    }
}
