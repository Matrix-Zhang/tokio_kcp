//! Library of KCP on Tokio

extern crate bytes;
#[macro_use]
extern crate futures;
extern crate kcp;
extern crate mio;
extern crate net2;
#[macro_use]
extern crate tokio_core;
extern crate time;
extern crate tokio_io;
extern crate tokio_proto;
extern crate tokio_service;
#[macro_use]
extern crate log;
extern crate priority_queue;

use time::Timespec;

pub use self::config::{KcpConfig, KcpNoDelayConfig};
pub use self::listener::{Incoming, KcpListener};
pub use self::server::KcpServer;
pub use self::session::{KcpSessionManager, KcpSessionUpdater};
pub use self::stream::{KcpStream, ServerKcpStream};

mod config;
mod debug;
mod kcp_io;
mod listener;
mod server;
mod session;
mod skcp;
mod stream;

#[inline]
fn as_millisec(timespec: &Timespec) -> u32 {
    (timespec.sec * 1000 + timespec.nsec as i64 / 1000 / 1000) as u32
}

#[inline]
fn current() -> u32 {
    let timespec = time::get_time();
    as_millisec(&timespec)
}
