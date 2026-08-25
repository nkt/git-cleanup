use std::sync::atomic::{AtomicBool, Ordering};

static DEBUG: AtomicBool = AtomicBool::new(false);

pub fn set(on: bool) {
    DEBUG.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

#[macro_export]
macro_rules! dbg_log {
    ($($arg:tt)*) => {
        if $crate::debug::enabled() {
            eprintln!("[debug] {}", format_args!($($arg)*));
        }
    };
}
