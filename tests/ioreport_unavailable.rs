//! IOReport can be missing altogether: in every virtual machine, or under a
//! sandbox that denies its user client. `--json` must then flag the SoC
//! figures as unavailable rather than print zeros that read as an idle chip.
//!
//! The situation is produced here with `sandbox-exec` and a profile that
//! denies nothing but `IOReportUserClient`.

use std::io::BufReader;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const NO_IOREPORT: &str = r#"(version 1)
(allow default)
(deny iokit-open (iokit-user-client-class "IOReportUserClient"))"#;

#[test]
fn json_flags_soc_as_unavailable_without_ioreport() {
    // A sandboxed process can't apply a second profile: nothing to test with
    // when the test run is itself sandboxed.
    let can_sandbox = Command::new(SANDBOX_EXEC)
        .args(["-p", NO_IOREPORT, "/usr/bin/true"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !can_sandbox {
        eprintln!("sandbox-exec can't apply a profile here: skipping");
        return;
    }

    let mut child = Command::new(SANDBOX_EXEC)
        .args(["-p", NO_IOREPORT, env!("CARGO_BIN_EXE_macpow")])
        .args(["--json", "--interval", "200"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("sandbox-exec should start macpow");
    let stdout = child.stdout.take().unwrap();

    // The read below blocks, so bound it: stop the child if it stays silent.
    let child = Arc::new(Mutex::new(child));
    let watchdog = Arc::clone(&child);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(30));
        let _ = watchdog.lock().unwrap().kill();
    });

    // IOReport takes a moment to fail (half a second or so, seconds on a
    // busy machine) and the snapshots until then carry no flag: read on
    // until one does.
    let deadline = Instant::now() + Duration::from_secs(20);
    let flagged = serde_json::Deserializer::from_reader(BufReader::new(stdout))
        .into_iter::<serde_json::Value>()
        .map_while(Result::ok)
        .take_while(|_| Instant::now() < deadline)
        .find(|m| m["soc"]["cpu_unavailable"] == true);

    {
        let mut child = child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }

    let m = flagged.expect("soc.cpu_unavailable should be set when IOReport can't be opened");
    let soc = &m["soc"];
    assert_eq!(soc["rails_unavailable"], true);
    assert_eq!(soc["total_w"], 0.0);
    assert_eq!(soc["ecpu_clusters"].as_array().map(Vec::len), Some(0));
}
