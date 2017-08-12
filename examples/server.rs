extern crate futures;
extern crate tokio_core;
extern crate tokio_kcp;
extern crate tokio_io;
extern crate env_logger;

use std::env;
use std::net::SocketAddr;

use futures::future::Future;
use futures::stream::Stream;
use tokio_core::reactor::Core;
use tokio_io::AsyncRead;
use tokio_io::io::copy;
use tokio_kcp::{KcpConfig, KcpListener, KcpNoDelayConfig};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TestMode {
    Default,
    Normal,
    Fast,
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
        .unwrap_or_else(|| "127.0.0.1:2233".to_string());
    let addr = addr.parse::<SocketAddr>().unwrap();

    let mode = env::args().nth(2).unwrap_or_else(|| "default".to_string());
    let mode = match &mode[..] {
        "default" => TestMode::Default,
        "normal" => TestMode::Normal,
        "fast" => TestMode::Fast,
        _ => panic!("Unrecognized mode {}", mode),
    };

    let mut core = Core::new().unwrap();
    let handle = core.handle();

    let config = get_config(mode);
    let listener = KcpListener::bind_with_config(&addr, &handle, config).unwrap();
    println!("listening on: {}", addr);

    let echo = listener.incoming().for_each(|(stream, addr)| {
        let (reader, writer) = stream.split();
        let amt = copy(reader, writer);
        let msg = amt.then(move |result| {
                               match result {
                                   Ok((amt, ..)) => println!("wrote {} bytes to {}", amt, addr),
                                   Err(e) => println!("error on {}: {}", addr, e),
                               }
                               Ok(())
                           });

        handle.spawn(msg);

        Ok(())
    });

    core.run(echo).unwrap();
}
