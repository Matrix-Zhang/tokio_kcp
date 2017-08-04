extern crate tokio_kcp;
#[macro_use]
extern crate tokio_core;
extern crate tokio_io;
extern crate env_logger;
extern crate futures;
extern crate bytes;

use std::io::{self, Cursor, Read, Write};
use std::net::SocketAddr;

use bytes::{Buf, BufMut, BytesMut, LittleEndian};
use futures::{Async, Future, Poll, Stream};
use tokio_core::reactor::Core;
use tokio_io::AsyncRead;
use tokio_io::io::copy;

use tokio_kcp::{KcpListener, KcpStream};

const TEST_PAYLOAD: &'static [u8] = b"HellO\x01WoRlD";

struct LoopSender<W: Write> {
    w: Option<W>,
    count: usize,
}

impl<W: Write> LoopSender<W> {
    fn new(w: W, count: usize) -> LoopSender<W> {
        LoopSender {
            w: Some(w),
            count: count,
        }
    }
}

impl<W: Write> Future for LoopSender<W> {
    type Item = W;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<W, io::Error> {
        while self.count > 0 {
            let mut buf = BytesMut::with_capacity(4 + TEST_PAYLOAD.len());
            buf.put_u32::<LittleEndian>(self.count as u32);
            buf.put(TEST_PAYLOAD);

            {
                let w = self.w.as_mut().unwrap();
                try_nb!(w.write_all(&buf));
            }

            self.count -= 1;
        }

        Ok(Async::Ready(self.w.take().unwrap()))
    }
}

struct LoopReader<R: Read> {
    r: Option<R>,
    count: usize,
}

impl<R: Read> LoopReader<R> {
    fn new(r: R, count: usize) -> LoopReader<R> {
        LoopReader {
            r: Some(r),
            count: count,
        }
    }
}

impl<R: Read> Future for LoopReader<R> {
    type Item = R;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<R, io::Error> {
        while self.count > 0 {
            let mut buf = vec![0u8; 4 + TEST_PAYLOAD.len()];

            {
                let r = self.r.as_mut().unwrap();
                try_nb!(r.read_exact(&mut buf));
            }

            let mut cur = Cursor::new(buf);
            let cnt = cur.get_u32::<LittleEndian>();
            assert_eq!(cnt as usize, self.count);

            self.count -= 1;
        }

        Ok(Async::Ready(self.r.take().unwrap()))
    }
}

#[test]
fn echo() {
    let _ = env_logger::init();

    let mut core = Core::new().unwrap();
    let handle = core.handle();

    let zerod = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
    let listener = KcpListener::bind(&zerod, &handle).unwrap();
    let addr = listener.local_addr().unwrap();

    let svr = listener.incoming().for_each(move |(s, _)| {
                                               let (r, w) = s.split();
                                               let fut = copy(r, w);
                                               handle.spawn(fut.map(|_| ()).map_err(|_| ()));
                                               Ok(())
                                           });

    let handle = core.handle();
    handle.spawn(svr.map_err(|err| {
                                 panic!("Failed to run server: {:?}", err);
                             }));

    let cli = KcpStream::connect(&addr, &handle).and_then(|s| {
                                                              let (r, w) = s.split();
                                                              let w_fut = LoopSender::new(w, 100);
                                                              let r_fut = LoopReader::new(r, 100);
                                                              r_fut.join(w_fut)
                                                          });

    core.run(cli).unwrap();
}
