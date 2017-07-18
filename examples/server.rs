extern crate futures;
extern crate tokio_core;
extern crate tokio_kcp;
extern crate tokio_io;

use std::env;
use std::net::SocketAddr;

use futures::future::Future;
use futures::stream::Stream;
use tokio_core::reactor::Core;
use tokio_kcp::KcpListener;
use tokio_io::AsyncRead;
use tokio_io::io::copy;

fn main() {
    let addr = env::args().nth(1).unwrap_or_else(
        || "127.0.0.1:2233".to_string(),
    );
    let addr = addr.parse::<SocketAddr>().unwrap();

    let mut core = Core::new().unwrap();
    let handle = core.handle();
    let listener = KcpListener::bind(&addr, &core.handle()).unwrap();
    println!("listening on: {}", addr);

    let echo = listener.incoming().for_each(|(stream, addr)| {
        let (reader, writer) = stream.split();
        let amt = copy(reader, writer);
        let msg = amt.then(move |result| {
            match result {
                Ok((amt, _, _)) => println!("wrote {} bytes to {}", amt, addr),
                Err(e) => println!("error on {}: {}", addr, e),
            }
            Ok(())
        });

        handle.spawn(msg);

        Ok(())
    });

    core.run(echo).unwrap();
}
