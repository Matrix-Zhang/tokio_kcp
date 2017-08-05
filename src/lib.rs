//! Library of KCP on Tokio

extern crate bytes;
#[macro_use]
extern crate futures;
extern crate kcp;
extern crate mio;
#[macro_use]
extern crate tokio_core;
extern crate tokio_io;
extern crate rand;
extern crate time;
#[macro_use]
extern crate log;

use time::Timespec;

pub use self::config::{KcpConfig, KcpNoDelayConfig};
pub use self::listener::{Incoming, KcpListener};
pub use self::stream::{KcpClientStream, KcpStream, KcpStreamNew};

mod skcp;
mod session;
mod kcp_io;
mod stream;
mod listener;
mod config;
mod debug;

#[inline]
fn as_millisec(timespec: &Timespec) -> u32 {
    (timespec.sec * 1000 + timespec.nsec as i64 / 1000 / 1000) as u32
}

#[inline]
fn current() -> u32 {
    let timespec = time::get_time();
    as_millisec(&timespec)
}
