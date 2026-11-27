#[cfg(not(target_os = "macos"))]
compile_error!("macpow supports macOS targets only.");

pub mod battery;
pub mod cf_utils;
pub mod clpc;
pub mod iokit_ffi;
pub mod ioreport;
pub mod metrics;
pub mod peripherals;
pub mod powermetrics;
pub mod process_utils;
pub mod sma;
pub mod smc;
pub mod topology;
pub mod types;
