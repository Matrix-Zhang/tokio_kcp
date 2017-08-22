extern crate bytes;
extern crate futures;
extern crate tokio_core;
extern crate tokio_kcp;
extern crate tokio_io;
extern crate env_logger;
extern crate log;
extern crate time;

use std::env;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::thread;

use bytes::{BufMut, BytesMut};
use env_logger::LogBuilder;
use futures::{Future, Sink, Stream};
use futures::future::Either;
use futures::sync::mpsc;
use log::LogRecord;
use tokio_core::reactor::Core;
use tokio_io::AsyncRead;
use tokio_io::codec::{Decoder, Encoder};
use tokio_kcp::{KcpSessionManager, KcpStream};

fn log_time(record: &LogRecord) -> String {
    format!("[{}][{}] {}", time::now().strftime("%Y-%m-%d][%H:%M:%S.%f").unwrap(), record.level(), record.args())
}

fn main() {
    let mut log_builder = LogBuilder::new();
    // Default filter
    log_builder.format(log_time);
    if let Ok(env_conf) = env::var("RUST_LOG") {
        log_builder.parse(&env_conf);
    }
    log_builder.init().unwrap();

    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:2233".into());
    let addr = addr.parse::<SocketAddr>().unwrap();

    let mut core = Core::new().unwrap();
    let handle = core.handle();
    let (stdin_tx, stdin_rx) = mpsc::channel(0);
    thread::spawn(|| read_stdin(stdin_tx));

    let stdin_rx = stdin_rx.map_err(|err| panic!("RX err: {:?}", err));
    let mut stdout = io::stdout();

    let mut updater = KcpSessionManager::new(&handle).unwrap();

    let client = futures::lazy(|| KcpStream::connect(0, &addr, &handle, &mut updater)).and_then(|stream| {
        let (sink, stream) = stream.framed(Bytes).split();
        let send_stdin = stdin_rx.forward(sink);
        let write_stdout = stream.for_each(move |buf| stdout.write_all(&buf));

        send_stdin.select2(write_stdout).then(|r| match r {
                                                  Ok(..) => Ok(()),
                                                  Err(Either::A((err, ..))) => Err(err),
                                                  Err(Either::B((err, ..))) => Err(err),
                                              })
    });

    core.run(client).unwrap();
}


struct Bytes;

impl Decoder for Bytes {
    type Item = BytesMut;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> io::Result<Option<BytesMut>> {
        if !buf.is_empty() {
            let len = buf.len();
            Ok(Some(buf.split_to(len)))
        } else {
            Ok(None)
        }
    }

    fn decode_eof(&mut self, buf: &mut BytesMut) -> io::Result<Option<BytesMut>> {
        self.decode(buf)
    }
}

impl Encoder for Bytes {
    type Item = Vec<u8>;
    type Error = io::Error;

    fn encode(&mut self, data: Vec<u8>, buf: &mut BytesMut) -> io::Result<()> {
        buf.put(&data[..]);
        Ok(())
    }
}

fn read_stdin(mut tx: mpsc::Sender<Vec<u8>>) {
    let mut stdin = io::stdin();
    loop {
        let mut buf = vec![0; 1024];
        let n = match stdin.read(&mut buf) {
            Ok(0) => break,
            Err(err) => panic!("Failed to read from stdin, err: {:?}", err),
            Ok(n) => n,
        };
        buf.truncate(n);
        tx = tx.send(buf).wait().unwrap();
    }
}
