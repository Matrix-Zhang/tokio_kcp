use std::{
    collections::{hash_map::Entry, HashMap},
    io::ErrorKind,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use kcp::KcpResult;
use log::{error, trace};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, Mutex},
    time::{self, Instant},
};

use crate::{skcp::KcpSocket, KcpConfig};

pub struct KcpSession {
    socket: Mutex<KcpSocket>,
    closed: AtomicBool,
    session_expire: Duration,
    session_close_notifier: Option<mpsc::Sender<u32>>,
}

impl KcpSession {
    fn new(
        socket: KcpSocket,
        session_expire: Duration,
        session_close_notifier: Option<mpsc::Sender<u32>>,
    ) -> KcpSession {
        KcpSession {
            socket: Mutex::new(socket),
            closed: AtomicBool::new(false),
            session_expire,
            session_close_notifier,
        }
    }

    pub fn new_shared(
        socket: KcpSocket,
        session_expire: Duration,
        session_close_notifier: Option<mpsc::Sender<u32>>,
    ) -> Arc<KcpSession> {
        let is_client = session_close_notifier.is_none();
        let session = Arc::new(KcpSession::new(socket, session_expire, session_close_notifier));

        {
            let session = session.clone();
            tokio::spawn(async move {
                let mut input_buffer = [0u8; 65536];

                loop {
                    let next = {
                        let mut socket = session.socket.lock().await;

                        let is_closed = session.closed.load(Ordering::Acquire);
                        if is_closed && socket.can_close() {
                            trace!("[SESSION] KCP session closed");
                            break;
                        }

                        if is_client {
                            // If this is a client stream, pull data from socket automatically

                            match socket.udp_socket().try_recv(&mut input_buffer) {
                                Ok(n) => {
                                    let input_buffer = &input_buffer[..n];
                                    if let Err(err) = socket.input(input_buffer) {
                                        error!("[SESSION] input failed, error: {}", err);
                                    }
                                }
                                Err(ref err) if err.kind() == ErrorKind::WouldBlock => {}
                                Err(err) => {
                                    error!("[SESSION] UDP recv failed, error: {}", err);
                                }
                            }
                        } else {
                            // If this is a server stream, close it automatically after a period of time
                            let last_update_time = socket.last_update_time();
                            let elapsed = last_update_time.elapsed();

                            if elapsed > session.session_expire {
                                if elapsed > session.session_expire * 2 {
                                    // Force close. Client may have already gone.
                                    trace!(
                                        "[SESSION] force close inactive session, conv: {}, last_update: {}s ago",
                                        socket.conv(),
                                        elapsed.as_secs()
                                    );
                                    break;
                                }

                                if !is_closed {
                                    trace!(
                                        "[SESSION] closing inactive session, conv: {}, last_update: {}s ago",
                                        socket.conv(),
                                        elapsed.as_secs()
                                    );
                                    session.closed.store(true, Ordering::Release);
                                }
                            }
                        }

                        match socket.update() {
                            Err(err) => {
                                error!("[SESSION] KCP.update failed, error: {}", err);
                                Instant::now() + Duration::from_secs(1)
                            }
                            Ok(next) => Instant::from_std(next),
                        }
                    };

                    time::sleep_until(next).await;
                }

                {
                    // Close the socket.
                    // Wake all pending tasks and let all send/recv return EOF

                    let mut socket = session.socket.lock().await;
                    socket.close();
                }

                if let Some(ref notifier) = session.session_close_notifier {
                    let socket = session.socket.lock().await;
                    let _ = notifier.send(socket.conv()).await;
                }
            });
        }

        session
    }

    pub fn kcp_socket(&self) -> &Mutex<KcpSocket> {
        &self.socket
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

pub struct KcpSessionManager {
    sessions: HashMap<u32, Arc<KcpSession>>,
    next_free_conv: u32,
}

impl KcpSessionManager {
    pub fn new() -> KcpSessionManager {
        KcpSessionManager {
            sessions: HashMap::new(),
            next_free_conv: 0,
        }
    }

    pub fn close_conv(&mut self, conv: u32) {
        self.sessions.remove(&conv);
    }

    pub fn alloc_conv(&mut self) -> u32 {
        loop {
            let (mut c, _) = self.next_free_conv.overflowing_add(1);
            if c == 0 {
                let (nc, _) = c.overflowing_add(1);
                c = nc;
            }
            self.next_free_conv = c;

            if self.sessions.get(&self.next_free_conv).is_none() {
                let conv = self.next_free_conv;
                return conv;
            }
        }
    }

    pub fn get_or_create(
        &mut self,
        config: &KcpConfig,
        conv: u32,
        udp: &Arc<UdpSocket>,
        peer_addr: SocketAddr,
        session_close_notifier: &mpsc::Sender<u32>,
    ) -> KcpResult<(Arc<KcpSession>, bool)> {
        match self.sessions.entry(conv) {
            Entry::Occupied(occ) => Ok((occ.get().clone(), false)),
            Entry::Vacant(vac) => {
                let socket = KcpSocket::new(config, conv, udp.clone(), peer_addr, config.stream)?;
                let session =
                    KcpSession::new_shared(socket, config.session_expire, Some(session_close_notifier.clone()));
                trace!("created session for conv: {}, peer: {}", conv, peer_addr);
                vac.insert(session.clone());
                Ok((session, true))
            }
        }
    }
}
