use std::{io::Write, time::Duration};

use kcp::Kcp;

/// Kcp Delay Config
#[derive(Debug, Clone, Copy)]
pub struct KcpNoDelayConfig {
    /// Enable nodelay
    pub nodelay: bool,
    /// Internal update interval (ms)
    pub interval: i32,
    /// ACK number to enable fast resend
    pub resend: i32,
    /// Disable congetion control
    pub nc: bool,
}

impl Default for KcpNoDelayConfig {
    fn default() -> KcpNoDelayConfig {
        KcpNoDelayConfig {
            nodelay: false,
            interval: 100,
            resend: 0,
            nc: false,
        }
    }
}

impl KcpNoDelayConfig {
    /// Get a fastest configuration
    ///
    /// 1. Enable NoDelay
    /// 2. Set ticking interval to be 10ms
    /// 3. Set fast resend to be 2
    /// 4. Disable congestion control
    pub fn fastest() -> KcpNoDelayConfig {
        KcpNoDelayConfig {
            nodelay: true,
            interval: 10,
            resend: 2,
            nc: true,
        }
    }

    /// Get a normal configuration
    ///
    /// 1. Disable NoDelay
    /// 2. Set ticking interval to be 40ms
    /// 3. Disable fast resend
    /// 4. Enable congestion control
    pub fn normal() -> KcpNoDelayConfig {
        KcpNoDelayConfig {
            nodelay: false,
            interval: 40,
            resend: 0,
            nc: false,
        }
    }
}

/// Kcp Config
#[derive(Debug, Clone, Copy)]
pub struct KcpConfig {
    /// Max Transmission Unit
    pub mtu: Option<usize>,
    /// nodelay
    pub nodelay: KcpNoDelayConfig,
    /// Send window size
    pub wnd_size: Option<(u16, u16)>,
    /// Minimal resend timeout
    pub rx_minrto: Option<u32>,
    /// Session expire duration, default is 90 seconds
    pub session_expire: Duration,
    /// Flush KCP state immediately after write
    pub flush_write: bool,
    /// Flush ACKs immediately after input
    pub flush_acks_input: bool,
    /// Stream mode
    pub stream: bool,
}

impl Default for KcpConfig {
    fn default() -> KcpConfig {
        KcpConfig {
            mtu: None,
            nodelay: KcpNoDelayConfig::normal(),
            wnd_size: None,
            rx_minrto: None,
            session_expire: Duration::from_secs(90),
            flush_write: false,
            flush_acks_input: false,
            stream: true,
        }
    }
}

impl KcpConfig {
    /// Applies config onto `Kcp`
    #[doc(hidden)]
    pub fn apply_config<W: Write>(&self, k: &mut Kcp<W>) {
        if let Some(mtu) = self.mtu {
            k.set_mtu(mtu).expect("Invalid MTU");
        }

        k.set_nodelay(
            self.nodelay.nodelay,
            self.nodelay.interval,
            self.nodelay.resend,
            self.nodelay.nc,
        );

        if let Some(rm) = self.rx_minrto {
            k.set_rx_minrto(rm);
        }

        if let Some(ws) = self.wnd_size {
            k.set_wndsize(ws.0, ws.1);
        }
    }
}
