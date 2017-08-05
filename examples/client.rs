extern crate bytes;
extern crate futures;
extern crate tokio_core;
extern crate tokio_kcp;
extern crate tokio_io;
extern crate env_logger;

use std::env;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::thread;

use bytes::{BufMut, BytesMut};
use futures::{Future, Sink, Stream};
use futures::sync::mpsc;
use tokio_core::reactor::Core;
use tokio_io::AsyncRead;
use tokio_io::codec::{Decoder, Encoder};
use tokio_kcp::KcpStream;

fn main() {
    let _ = env_logger::init();

    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:2233".into());
    let addr = addr.parse::<SocketAddr>().unwrap();

    let mut core = Core::new().unwrap();
    let handle = core.handle();
    let (stdin_tx, stdin_rx) = mpsc::channel(0);
    thread::spawn(|| read_stdin(stdin_tx));

    let stdin_rx = stdin_rx.map_err(|_| panic!());
    let mut stdout = io::stdout();

    let client = futures::lazy(|| KcpStream::connect(&addr, &handle)).and_then(|stream| {
        let (sink, stream) = stream.framed(Bytes).split();
        let send_stdin = stdin_rx.forward(sink);
        let write_stdout = stream.for_each(move |buf| stdout.write_all(&buf));

        send_stdin.map(|_| ())
                  .join(write_stdout.map(|_| ()))
                  .map_err(|err| {
                               panic!("Failed to handle stream, err: {:?}", err);
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
