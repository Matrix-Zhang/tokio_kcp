extern crate bytes;
#[macro_use]
extern crate futures;
extern crate tokio_core;
extern crate tokio_kcp;
#[macro_use]
extern crate tokio_io;
extern crate env_logger;
#[macro_use]
extern crate log;
extern crate time;
extern crate rand;

use std::env;
use std::io::{self, Cursor, Read, Write};
use std::net::SocketAddr;
use std::time::Duration;

use bytes::{Buf, BufMut, BytesMut, LittleEndian};
use futures::{Async, Future, Poll, Stream};
use futures::future::Either;
use time::Timespec;
use tokio_core::reactor::{Core, Handle, Interval};
use tokio_io::AsyncRead;
use tokio_kcp::{KcpConfig, KcpNoDelayConfig, KcpSessionManager, KcpStream};

#[inline]
fn as_millisec(timespec: &Timespec) -> u32 {
    (timespec.sec * 1000 + timespec.nsec as i64 / 1000 / 1000) as u32
}

#[inline]
fn current() -> u32 {
    let timespec = time::get_time();
    as_millisec(&timespec)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TestMode {
    Default,
    Normal,
    Fast,
}

#[allow(dead_code)]
struct LoopSender<W: Write> {
    w: Option<W>,
    count: usize,
    intv: Interval,
    index: usize,
}

impl<W: Write> LoopSender<W> {
    fn new(w: W, count: usize, handle: &Handle) -> LoopSender<W> {
        LoopSender {
            w: Some(w),
            count: count,
            intv: Interval::new(Duration::from_millis(20), handle).expect("Failed to create interval"),
            index: 0,
        }
    }
}

impl<W: Write> Future for LoopSender<W> {
    type Item = W;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<W, io::Error> {
        let cur = current();
        loop {
            try_ready!(self.intv.poll());

            let mut buf = BytesMut::with_capacity(8);
            buf.put_u32::<LittleEndian>(self.index as u32);
            buf.put_u32::<LittleEndian>(cur);

            {
                let w = self.w.as_mut().unwrap();
                try_nb!(w.write_all(&buf));
            }

            self.index += 1;
        }

        // Ok(Async::Ready(self.w.take().unwrap()))
    }
}

struct LoopReader<R: Read> {
    r: Option<R>,
    count: usize,
    next: usize,
    max_rtt: usize,
    sum_rtt: usize,
    buf: Vec<u8>,
    mode: TestMode,
    start_ts: u32,
    i: usize,
}

impl<R: Read> LoopReader<R> {
    fn new(i: usize, mode: TestMode, r: R, count: usize) -> LoopReader<R> {
        LoopReader {
            r: Some(r),
            count: count,
            next: 0,
            max_rtt: 0,
            sum_rtt: 0,
            buf: vec![0u8; 8],
            mode: mode,
            start_ts: current(),
            i: i,
        }
    }
}

impl<R: Read> Drop for LoopReader<R> {
    fn drop(&mut self) {
        println!("{} {:?} mode result ({}ms)", self.i, self.mode, current() - self.start_ts);
        println!("{} avgrtt={} maxrtt={}", self.i, self.sum_rtt / self.count, self.max_rtt);
    }
}

impl<R: Read> Future for LoopReader<R> {
    type Item = R;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<R, io::Error> {
        let ccur = current();

        while self.next < self.count {
            {
                let r = self.r.as_mut().unwrap();
                try_nb!(r.read_exact(&mut self.buf));
            }

            let mut cur = Cursor::new(&self.buf);
            let sn = cur.get_u32::<LittleEndian>();
            let ts = cur.get_u32::<LittleEndian>();
            assert_eq!(sn as usize, self.next);

            self.next += 1;

            let rtt = ccur - ts;
            debug!("[RECV] {} mode={:?} sn={} rtt={}", self.i, self.mode, sn, rtt);

            self.sum_rtt += rtt as usize;
            if rtt as usize > self.max_rtt {
                self.max_rtt = rtt as usize;
            }
        }

        Ok(Async::Ready(self.r.take().unwrap()))
    }
}

fn get_config(mode: TestMode) -> KcpConfig {
    let mut config = KcpConfig::default();
    config.wnd_size = Some((128, 128));
    match mode {
        TestMode::Default => {
            config.nodelay = Some(KcpNoDelayConfig {
                                      nodelay: false,
                                      interval: 10,
                                      resend: 0,
                                      nc: false,
                                  });
        }
        TestMode::Normal => {
            config.nodelay = Some(KcpNoDelayConfig {
                                      nodelay: false,
                                      interval: 10,
                                      resend: 0,
                                      nc: true,
                                  });
        }
        TestMode::Fast => {
            config.nodelay = Some(KcpNoDelayConfig {
                                      nodelay: true,
                                      interval: 10,
                                      resend: 2,
                                      nc: true,
                                  });

            config.rx_minrto = Some(10);
            config.fast_resend = Some(1);
        }
    }

    config
}

fn main() {
    let _ = env_logger::init();

    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:2233".into());
    let addr = addr.parse::<SocketAddr>().unwrap();

    let mode = env::args().nth(2).unwrap_or_else(|| "default".to_string());
    let mode = match &mode[..] {
        "default" => TestMode::Default,
        "normal" => TestMode::Normal,
        "fast" => TestMode::Fast,
        _ => panic!("Unrecognized mode {}", mode),
    };

    let count = env::args()
        .nth(3)
        .unwrap_or_else(|| "1".to_owned())
        .parse::<usize>()
        .unwrap();

    let config = get_config(mode);

    let mut core = Core::new().unwrap();

    let mut fut: Option<Box<Future<Item = (), Error = io::Error>>> = None;

    let handle = core.handle();
    let mut updater = KcpSessionManager::new(&handle).unwrap();

    for i in 0..count {
        let handle = core.handle();
        let chandle = handle.clone();

        let mut updater = updater.clone();
        let cli = futures::lazy(move || KcpStream::connect_with_config(0, &addr, &handle, &mut updater, &config))
            .and_then(move |s| {
                let (r, w) = s.split();
                let w_fut = LoopSender::new(w, 1000, &chandle);
                let r_fut = LoopReader::new(i, TestMode::Default, r, 1000);
                // r_fut.join(w_fut)
                r_fut.select2(w_fut).then(|r| match r {
                                              Ok(..) => Ok(()),
                                              Err(Either::A((err, ..))) |
                                              Err(Either::B((err, ..))) => Err(err),
                                          })
            });

        match fut.take() {
            Some(f) => {
                fut = Some(Box::new(f.join(cli).map(|_| ())) as Box<Future<Item = (), Error = io::Error>>);
            }
            None => fut = Some(Box::new(cli) as Box<Future<Item = (), Error = io::Error>>),
        }
    }

    let fut = fut.unwrap();
    let fut = fut.then(|_| {
                           updater.stop();
                           Ok::<(), ()>(())
                       });
    core.run(fut).unwrap();
}
