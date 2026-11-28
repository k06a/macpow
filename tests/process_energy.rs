//! Hardware tests for per-process energy: need bare-metal Apple Silicon, where
//! the kernel accounts CPU energy per task. Run with
//! `cargo test --test process_energy`.

use macpow::process_power::{read_all_process_energy, ProcessTracker};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A child process that keeps one core busy. Killed and reaped on drop, so a
/// failing assertion cannot leave it running.
struct BusyChild(Child);

impl BusyChild {
    fn spawn() -> Self {
        let child = Command::new("/usr/bin/yes")
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn /usr/bin/yes");
        Self(child)
    }

    fn pid(&self) -> i32 {
        self.0.id() as i32
    }

    /// Cumulative energy of the child in nanojoules, as macpow reads it.
    fn energy_nj(&self) -> u64 {
        read_all_process_energy()
            .get(&self.pid())
            .expect("a running child of ours must be in the scan")
            .energy_nj
    }
}

impl Drop for BusyChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Virtual machines do not expose per-task energy; the counters stay at zero.
fn in_vm() -> bool {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of_val(&value);
    // SAFETY: the name is NUL-terminated, and `value`/`len` describe a
    // writable buffer of exactly the size the kernel is told about.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.hv_vmm_present".as_ptr(),
            (&raw mut value).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    rc == 0 && value != 0
}

#[test]
fn busy_child_energy_counter_advances() {
    if in_vm() {
        eprintln!("skipped: no per-task energy accounting inside a VM");
        return;
    }
    let child = BusyChild::spawn();
    // Let the child get through exec and onto a core.
    std::thread::sleep(Duration::from_millis(200));

    let before = child.energy_nj();
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs(1));
    let after = child.energy_nj();
    let watts = after.saturating_sub(before) as f64 / 1e9 / started.elapsed().as_secs_f64();

    // One busy core is 1-3 W here (M5 Max); the bounds only have to tell a
    // live counter from a frozen one and from a unit mistake.
    assert!(
        (0.1..50.0).contains(&watts),
        "a process at 100% CPU should show plausible power, got {watts:.4} W \
         (counter went {before} -> {after} nJ)"
    );
}

#[test]
fn tracker_follows_a_busy_child_through_its_death() {
    if in_vm() {
        eprintln!("skipped: no per-task energy accounting inside a VM");
        return;
    }
    let child = BusyChild::spawn();
    let pid = child.pid();
    std::thread::sleep(Duration::from_millis(200));

    let mut tracker = ProcessTracker::default();
    tracker.update(&read_all_process_energy(), Instant::now());
    std::thread::sleep(Duration::from_secs(1));
    let report = tracker.update(&read_all_process_energy(), Instant::now());
    let row = report
        .processes
        .iter()
        .find(|p| p.pid == pid)
        .expect("a process at 100% CPU must be among the top processes");
    assert!(row.alive);
    assert!(
        (0.1..50.0).contains(&row.power_w),
        "implausible power for one busy core: {} W",
        row.power_w
    );
    assert!(report.total_power_w >= row.power_w);
    let (energy_mj, total_mj) = (row.energy_mj, report.total_energy_mj);
    assert!(energy_mj > 0.0);

    drop(child); // killed and reaped: the pid is gone from the next scan
    let report = tracker.update(&read_all_process_energy(), Instant::now());
    let row = report
        .processes
        .iter()
        .find(|p| p.pid == pid)
        .expect("an exited process stays listed for a while");
    assert!(!row.alive);
    assert_eq!(row.power_w, 0.0, "an exited process draws nothing");
    assert_eq!(row.energy_mj, energy_mj, "its session energy is kept");
    assert!(report.total_energy_mj >= total_mj);
}
