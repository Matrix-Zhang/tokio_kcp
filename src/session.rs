use std::{
    collections::{hash_map::Entry, HashMap},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use byte_string::ByteStr;
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
    input_tx: mpsc::Sender<Vec<u8>>,
}

impl KcpSession {
    fn new(
        socket: KcpSocket,
        session_expire: Duration,
        session_close_notifier: Option<mpsc::Sender<u32>>,
        input_tx: mpsc::Sender<Vec<u8>>,
    ) -> KcpSession {
        KcpSession {
            socket: Mutex::new(socket),
            closed: AtomicBool::new(false),
            session_expire,
            session_close_notifier,
            input_tx,
        }
    }

    pub fn new_shared(
        socket: KcpSocket,
        session_expire: Duration,
        session_close_notifier: Option<mpsc::Sender<u32>>,
    ) -> Arc<KcpSession> {
        let is_client = session_close_notifier.is_none();

        let (input_tx, mut input_rx) = mpsc::channel(64);

        let udp_socket = socket.udp_socket().clone();

        let session = Arc::new(KcpSession::new(
            socket,
            session_expire,
            session_close_notifier,
            input_tx,
        ));

        {
            let session = session.clone();
            tokio::spawn(async move {
                let mut input_buffer = [0u8; 65536];
                let update_timer = time::sleep(Duration::from_millis(10));
                tokio::pin!(update_timer);

                loop {
                    tokio::select! {
                        // recv() then input()
                        // Drives the KCP machine forward
                        recv_result = udp_socket.recv(&mut input_buffer), if is_client => {
                            match recv_result {
                                Err(err) => {
                                    error!("[SESSION] UDP recv failed, error: {}", err);
                                }
                                Ok(n) => {
                                    let input_buffer = &input_buffer[..n];
                                    trace!("[SESSION] UDP recv {} bytes, going to input {:?}", n, ByteStr::new(input_buffer));

                                    let mut socket = session.socket.lock().await;

                                    match socket.input(input_buffer) {
                                        Ok(true) => {
                                            trace!("[SESSION] UDP input {} bytes and waked sender/receiver", n);
                                        }
                                        Ok(false) => {}
                                        Err(err) => {
                                            error!("[SESSION] UDP input {} bytes error: {}, input buffer {:?}", n, err, ByteStr::new(input_buffer));
                                        }
                                    }
                                }
                            }
                        }

                        // bytes received from listener socket
                        input_opt = input_rx.recv() => {
                            if let Some(input_buffer) = input_opt {
                                let mut socket = session.socket.lock().await;
                                match socket.input(&input_buffer) {
                                    Ok(..) => {
                                        trace!("[SESSION] UDP input {} bytes from channel {:?}", input_buffer.len(), ByteStr::new(&input_buffer));
                                    }
                                    Err(err) => {
                                        error!("[SESSION] UDP input {} bytes from channel failed, error: {}, input buffer {:?}",
                                               input_buffer.len(), err, ByteStr::new(&input_buffer));
                                    }
                                }
                            }
                        }

                        // Call update() in period
                        _ = &mut update_timer => {
                            let mut socket = session.socket.lock().await;

                            let is_closed = session.closed.load(Ordering::Acquire);
                            if is_closed && socket.can_close() {
                                trace!("[SESSION] KCP session closed");
                                break;
                            }

                            // server socket expires
                            if !is_client {
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
                                Ok(next_next) => {
                                    update_timer.as_mut().reset(Instant::from_std(next_next));
                                }
                                Err(err) => {
                                    error!("[SESSION] KCP update failed, error: {}", err);
                                    update_timer.as_mut().reset(Instant::now() + Duration::from_millis(10));
                                }
                            }
                        }
                    }
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

    pub async fn input(&self, buf: &[u8]) {
        self.input_tx.send(buf.to_owned()).await.expect("input channel closed")
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
