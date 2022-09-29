use std::time::{SystemTime, UNIX_EPOCH};

#[inline]
pub fn now_millis() -> u32 {
    let start = SystemTime::now();
    let since_the_epoch = start.duration_since(UNIX_EPOCH).expect("time went afterwards");
    // (since_the_epoch.as_secs() * 1000 + since_the_epoch.subsec_millis() as u64) as u32
    since_the_epoch.as_millis() as u32
}
