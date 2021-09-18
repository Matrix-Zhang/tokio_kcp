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
}

/// Kcp Config
#[derive(Debug, Clone, Copy)]
pub struct KcpConfig {
    /// Max Transmission Unit
    pub mtu: Option<usize>,
    /// Internal update interval
    pub interval: Option<u32>,
    /// nodelay
    pub nodelay: Option<KcpNoDelayConfig>,
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
            interval: None,
            nodelay: None,
            wnd_size: None,
            rx_minrto: None,
            session_expire: Duration::from_secs(90),
            flush_write: true,
            flush_acks_input: true,
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

        if let Some(interval) = self.interval {
            k.set_interval(interval);
        }

        if let Some(ref nodelay) = self.nodelay {
            k.set_nodelay(nodelay.nodelay, nodelay.interval, nodelay.resend, nodelay.nc);
        }

        if let Some(rm) = self.rx_minrto {
            k.set_rx_minrto(rm);
        }

        if let Some(ws) = self.wnd_size {
            k.set_wndsize(ws.0, ws.1);
        }
    }
}
