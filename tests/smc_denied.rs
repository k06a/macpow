//! What the sampler still reports when the SMC cannot be opened, as in a
//! macOS virtual machine. The test denies the SMC to its own process with a
//! sandbox profile; a sandbox cannot be lifted again, which is why this test
//! lives alone in its file (each test file is a separate process).

use macpow::metrics::Sampler;
use macpow::smc::SmcConnection;
use std::ffi::{c_char, c_int, CString};
use std::time::{Duration, Instant};

extern "C" {
    // <sandbox.h>; with flags 0 the profile is the profile source text.
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
}

const DENY_SMC: &str =
    r#"(version 1)(allow default)(deny iokit-open (iokit-user-client-class "AppleSMCClient"))"#;

#[test]
fn cpu_and_memory_are_sampled_without_the_smc() {
    let profile = CString::new(DENY_SMC).unwrap();
    let mut error: *mut c_char = std::ptr::null_mut();
    // SAFETY: `profile` is a NUL-terminated string that outlives the call and
    // `error` is a valid out-pointer; its message is never read, only leaked.
    let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut error) };
    if rc != 0 {
        // Already inside a sandbox (nested ones are refused): nothing to test.
        eprintln!("skipped: cannot apply a sandbox profile to this process");
        return;
    }
    assert!(
        SmcConnection::open().is_err(),
        "the sandbox profile no longer denies the SMC"
    );

    let sampler = Sampler::new(100);
    // SAFETY: sysconf takes no pointers and has no preconditions.
    let logical_cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) } as usize;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut m = sampler.snapshot();
    // CPU usage needs two tick readings, one sampling interval apart.
    while m.cpu_usage_pct.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        m = sampler.snapshot();
    }

    assert_eq!(m.cpu_usage_pct.len(), logical_cpus);
    assert!(m.mem_used_gb > 0.5, "mem_used_gb = {}", m.mem_used_gb);
    assert!(
        m.temperatures.is_empty() && m.fans.is_empty(),
        "SMC readings appeared although the SMC is denied"
    );
}
