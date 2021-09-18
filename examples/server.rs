use std::{net::SocketAddr, time::Duration};

use byte_string::ByteStr;
use log::{debug, error, info};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time,
};
use tokio_kcp::{KcpConfig, KcpListener};

#[tokio::main]
async fn main() {
    env_logger::init();

    let config = KcpConfig::default();

    let server_addr = "127.0.0.1:3100".parse::<SocketAddr>().unwrap();

    let mut listener = KcpListener::bind(config, server_addr).await.unwrap();

    loop {
        let (mut stream, peer_addr) = match listener.accept().await {
            Ok(s) => s,
            Err(err) => {
                error!("accept failed, error: {}", err);
                time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        info!("accepted {}", peer_addr);

        tokio::spawn(async move {
            let mut buffer = [0u8; 8192];
            while let Ok(n) = stream.read(&mut buffer).await {
                debug!("recv {:?}", ByteStr::new(&buffer[..n]));
                if n == 0 {
                    break;
                }
                stream.write_all(&buffer[..n]).await.unwrap();
                debug!("echo {:?}", ByteStr::new(&buffer[..n]));
            }

            debug!("client {} closed", peer_addr);
        });
    }
}
