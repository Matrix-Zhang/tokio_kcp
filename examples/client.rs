use std::{net::SocketAddr, str};

use tokio::io::{stdin, AsyncReadExt, AsyncWriteExt};
use tokio_kcp::{KcpConfig, KcpStream};

#[tokio::main]
async fn main() {
    env_logger::init();

    let config = KcpConfig::default();

    let server_addr = "127.0.0.1:3100".parse::<SocketAddr>().unwrap();

    let mut stream = KcpStream::connect(&config, server_addr).await.unwrap();

    let mut buffer = [0u8; 8192];
    let mut i = stdin();
    loop {
        let n = i.read(&mut buffer).await.unwrap();
        stream.write_all(&buffer[..n]).await.unwrap();

        let n = stream.read(&mut buffer).await.unwrap();
        println!("{}", unsafe { str::from_utf8_unchecked(&buffer[..n]) });
    }
}
