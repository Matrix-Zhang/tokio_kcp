
extern crate bytes;
extern crate futures;
extern crate kcp;
extern crate mio;
#[macro_use]
extern crate tokio_core;
extern crate tokio_io;
extern crate rand;
extern crate time;

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use bytes::buf::FromBuf;
use futures::{Poll, Async, Future};
use futures::stream::Stream;
use kcp::prelude::*;
use mio::{Ready, Registration, PollOpt, Token, SetReadiness};
use mio::event::Evented;
use tokio_core::net::UdpSocket;
use tokio_core::reactor::{Handle, PollEvented, Timeout};
use tokio_io::{AsyncRead, AsyncWrite};

#[inline]
fn current() -> u32 {
    let timespec = time::get_time();
    (timespec.sec * 1000 + timespec.nsec as i64 / 1000 / 1000) as u32
}

pub struct KcpOutput {
    udp: Rc<UdpSocket>,
    peer: SocketAddr,
}

impl Write for KcpOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.udp.send_to(buf, &self.peer)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
struct KcpSession {
    kcp: Rc<RefCell<Kcp<KcpOutput>>>,
    timer: Rc<RefCell<Timeout>>,
    elapsed: Rc<RefCell<Instant>>,
    set_readiness: SetReadiness,
    owner: Option<(SocketAddr, Rc<RefCell<HashMap<SocketAddr, KcpSession>>>)>,
}

impl Stream for KcpSession {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<()>, io::Error> {
        let mut timer = self.timer.borrow_mut();
        match timer.poll() {
            Ok(Async::Ready(())) => {
                let mut kcp = self.kcp.borrow_mut();
                if self.elapsed.borrow().elapsed().as_secs() < 90 {
                    kcp.update(current())?;
                    let duration = Duration::from_millis(kcp.check(current()) as u64);
                    let next = Instant::now() + duration;
                    timer.reset(next);
                    Ok(Async::Ready(Some(())))
                } else {
                    kcp.expired();
                    self.set_readiness.set_readiness(Ready::readable())?;
                    if let Some(ref owner) = self.owner {
                        owner.1.borrow_mut().remove(&owner.0);
                    }
                    Ok(Async::NotReady)
                }
            }
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(e) => Err(e),
        }
    }
}

struct KcpIo {
    kcp: Rc<RefCell<Kcp<KcpOutput>>>,
    registration: Registration,
    set_readiness: SetReadiness,
}

impl Read for KcpIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.kcp.borrow_mut().recv(buf)
    }
}

impl Write for KcpIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut kcp = self.kcp.borrow_mut();
        kcp.send(&mut BytesMut::from_buf(buf)).and_then(|n| {
            kcp.flush().map(|_| n)
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Evented for KcpIo {
    fn register(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        self.registration.register(poll, token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        self.registration.reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
        <Registration as Evented>::deregister(&self.registration, poll)
    }
}

pub struct KcpClientStream {
    udp: Rc<UdpSocket>,
    io: PollEvented<KcpIo>,
    elapsed: Rc<RefCell<Instant>>,
}

impl Read for KcpClientStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Ok((n, _)) = self.udp.recv_from(buf) {
            let elapsed = self.elapsed.borrow().elapsed();
            *self.elapsed.borrow_mut() += elapsed;
            self.io.get_ref().kcp.borrow_mut().input(
                &mut BytesMut::from_buf(
                    &buf[..n],
                ),
            )?;
            self.io.get_ref().set_readiness.set_readiness(
                Ready::readable(),
            )?;
        }
        self.io.read(buf)
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
        self.io.get_ref().set_readiness.set_readiness(
            mio::Ready::writable(),
        )?;
        self.io.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

pub struct KcpStreamNew {
    inner: Option<KcpClientStream>,
}

impl Future for KcpStreamNew {
    type Item = KcpClientStream;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<KcpClientStream, io::Error> {
        Ok(Async::Ready(self.inner.take().unwrap()))
    }
}

pub struct KcpStream {
    io: PollEvented<KcpIo>,
}

impl KcpStream {
    pub fn connect(addr: &SocketAddr, handle: &Handle) -> KcpStreamNew {
        let local: SocketAddr = "0.0.0.0:0".parse().unwrap();

        let (registration, set_readiness) = Registration::new2();

        let timer = match Timeout::new_at(Instant::now(), handle) {
            Ok(timeout) => Rc::new(RefCell::new(timeout)),
            _ => return KcpStreamNew { inner: None },
        };

        let udp = match UdpSocket::bind(&local, handle) {
            Ok(udp) => Rc::new(udp),
            _ => return KcpStreamNew { inner: None },
        };

        let kcp = Rc::new(RefCell::new(Kcp::new(
            rand::random::<u32>(),
            KcpOutput {
                udp: udp.clone(),
                peer: *addr,
            },
        )));

        let io = KcpIo {
            kcp: kcp.clone(),
            registration: registration,
            set_readiness: set_readiness.clone(),
        };

        let io = match PollEvented::new(io, handle) {
            Ok(evented) => evented,
            _ => return KcpStreamNew { inner: None },
        };

        let elapsed = Rc::new(RefCell::new(Instant::now()));

        let interval = KcpSession {
            kcp: kcp.clone(),
            timer: timer.clone(),
            elapsed: elapsed.clone(),
            set_readiness: set_readiness.clone(),
            owner: None,
        };

        handle.spawn(interval.for_each(|_| Ok(())).then(|_| Ok(())));

        KcpStreamNew {
            inner: Some(KcpClientStream {
                udp: udp,
                io: io,
                elapsed: elapsed,
            }),
        }
    }
}

impl Read for KcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io.read(buf)
    }
}

impl Write for KcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.get_ref().set_readiness.set_readiness(
            mio::Ready::writable(),
        )?;
        self.io.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

impl AsyncRead for KcpStream {}

impl AsyncWrite for KcpStream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(().into())
    }
}

pub struct KcpListener {
    udp: Rc<UdpSocket>,
    sessions: Rc<RefCell<HashMap<SocketAddr, KcpSession>>>,
    handle: Handle,
}

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
    pub fn bind(addr: &SocketAddr, handle: &Handle) -> io::Result<KcpListener> {
        UdpSocket::bind(addr, handle).map(|udp| {
            KcpListener {
                udp: Rc::new(udp),
                sessions: Rc::new(RefCell::new(HashMap::new())),
                handle: handle.clone(),
            }
        })
    }

    pub fn accept(&mut self) -> io::Result<(KcpStream, SocketAddr)> {
        let mut buf = [0; 1500];

        loop {
            let (size, addr) = self.udp.recv_from(&mut buf)?;

            if let Some(session) = self.sessions.borrow_mut().get_mut(&addr) {
                let elapsed = session.elapsed.borrow().elapsed();
                *session.elapsed.borrow_mut() += elapsed;
                session.kcp.borrow_mut().input(&mut BytesMut::from_buf(
                    &buf[..size],
                ))?;
                session.set_readiness.set_readiness(Ready::readable())?;
                continue;
            }

            let kcp = Kcp::new(
                get_conv(&buf),
                KcpOutput {
                    udp: self.udp.clone(),
                    peer: addr,
                },
            );

            let kcp = Rc::new(RefCell::new(kcp));

            let (registration, set_readiness) = Registration::new2();

            let timer = Rc::new(RefCell::new(Timeout::new_at(Instant::now(), &self.handle)?));

            let io = KcpIo {
                kcp: kcp.clone(),
                registration: registration,
                set_readiness: set_readiness.clone(),
            };

            let session = KcpSession {
                kcp: kcp.clone(),
                timer: timer.clone(),
                elapsed: Rc::new(RefCell::new(Instant::now())),
                set_readiness: set_readiness.clone(),
                owner: Some((addr, self.sessions.clone())),
            };

            self.handle.spawn(
                session.clone().for_each(|_| Ok(())).then(
                    |_| Ok(()),
                ),
            );
            let io = PollEvented::new(io, &self.handle).unwrap();
            let stream = KcpStream { io: io };
            stream.io.get_ref().kcp.borrow_mut().input(
                &mut BytesMut::from_buf(&buf[..size]),
            )?;

            stream.io.get_ref().set_readiness.set_readiness(
                mio::Ready::readable(),
            )?;

            self.sessions.borrow_mut().insert(addr, session);
            return Ok((stream, addr));
        }
    }

    pub fn incoming(self) -> Incoming {
        Incoming { inner: self }
    }
}
