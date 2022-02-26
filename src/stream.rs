use std::{
    io::{self, ErrorKind},
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures::{future, ready};
use kcp::{Error as KcpError, KcpResult};
use log::trace;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::UdpSocket,
};

use crate::{config::KcpConfig, session::KcpSession, skcp::KcpSocket};

pub struct KcpStream {
    session: Arc<KcpSession>,
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
        let conv = rand::random();
        let socket = KcpSocket::new(config, conv, udp, addr, config.stream)?;

        let session = KcpSession::new_shared(socket, config.session_expire, None);

        Ok(KcpStream::with_session(session))
    }

    pub(crate) fn with_session(session: Arc<KcpSession>) -> KcpStream {
        KcpStream {
            session,
            recv_buffer: Vec::new(),
            recv_buffer_pos: 0,
            recv_buffer_cap: 0,
        }
    }

    pub fn poll_send(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<KcpResult<usize>> {
        // Mutex doesn't have poll_lock, spinning on it.
        let mut kcp = self.session.kcp_socket().lock();
        let result = ready!(kcp.poll_send(cx, buf));
        self.session.notify();
        result.into()
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
            let mut kcp = self.session.kcp_socket().lock();

            // Try to read from KCP
            // 1. Read directly with user provided `buf`
            match ready!(kcp.poll_recv(cx, buf)) {
                Ok(n) => {
                    trace!("[CLIENT] recv directly {} bytes", n);
                    self.session.notify();
                    return Ok(n).into();
                }
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
                    trace!("[CLIENT] recv buffered {} bytes", n);
                    self.session.notify();
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

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Mutex doesn't have poll_lock, spinning on it.
        let mut kcp = self.session.kcp_socket().lock();
        match kcp.flush() {
            Ok(..) => {
                self.session.notify();
                Ok(()).into()
            }
            Err(KcpError::IoError(err)) => Err(err).into(),
            Err(err) => Err(io::Error::new(ErrorKind::Other, err)).into(),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Ok(()).into()
    }
}
