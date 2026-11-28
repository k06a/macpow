//! The helper tools macpow still runs are started by absolute path, so what
//! it reports must not depend on the PATH it is launched with.
//! Needs a real Mac with some network activity, like tests/integration.rs.
//!
//! Run with: cargo test --test tool_paths

#[test]
fn per_process_network_counters_do_not_depend_on_path() {
    // Process-wide, which is why this file holds a single test.
    std::env::set_var("PATH", "/nonexistent");

    let counters = macpow::powermetrics::read_proc_net_counters();

    assert!(
        !counters.is_empty(),
        "nettop was not found, or no process has sent or received a byte"
    );
}
