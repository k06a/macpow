//! Per-process power.
//!
//! The kernel keeps a cumulative energy counter for every process
//! (`proc_pid_rusage`). [`read_all_process_energy`] reads those counters and
//! [`ProcessTracker`] turns successive readings into power and session energy.

use crate::types::ProcessPower;
use libc::c_int;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

// ── Kernel counters ──────────────────────────────────────────────────────────

/// `struct rusage_info_v6` from `<sys/resource.h>`, field for field.
///
/// `rusage_info_v4` is a strict prefix of it (everything up to and including
/// `ri_runnable_time`, 296 bytes), so one buffer serves both flavors.
// Most fields are never read; they are here to keep the layout exact.
#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RusageInfoV6 {
    ri_uuid: [u8; 16],
    ri_user_time: u64,
    ri_system_time: u64,
    ri_pkg_idle_wkups: u64,
    ri_interrupt_wkups: u64,
    ri_pageins: u64,
    ri_wired_size: u64,
    ri_resident_size: u64,
    ri_phys_footprint: u64,
    ri_proc_start_abstime: u64,
    ri_proc_exit_abstime: u64,
    ri_child_user_time: u64,
    ri_child_system_time: u64,
    ri_child_pkg_idle_wkups: u64,
    ri_child_interrupt_wkups: u64,
    ri_child_pageins: u64,
    ri_child_elapsed_abstime: u64,
    ri_diskio_bytesread: u64,
    ri_diskio_byteswritten: u64,
    ri_cpu_time_qos_default: u64,
    ri_cpu_time_qos_maintenance: u64,
    ri_cpu_time_qos_background: u64,
    ri_cpu_time_qos_utility: u64,
    ri_cpu_time_qos_legacy: u64,
    ri_cpu_time_qos_user_initiated: u64,
    ri_cpu_time_qos_user_interactive: u64,
    ri_billed_system_time: u64,
    ri_serviced_system_time: u64,
    ri_logical_writes: u64,
    ri_lifetime_max_phys_footprint: u64,
    ri_instructions: u64,
    ri_cycles: u64,
    ri_billed_energy: u64,
    ri_serviced_energy: u64,
    ri_interval_max_phys_footprint: u64,
    ri_runnable_time: u64,
    // ── end of rusage_info_v4 ──
    ri_flags: u64,
    // ── end of rusage_info_v5 ──
    ri_user_ptime: u64,
    ri_system_ptime: u64,
    ri_pinstructions: u64,
    ri_pcycles: u64,
    ri_energy_nj: u64,
    ri_penergy_nj: u64,
    ri_secure_time_in_system: u64,
    ri_secure_ptime_in_system: u64,
    ri_neural_footprint: u64,
    ri_lifetime_max_neural_footprint: u64,
    ri_interval_max_neural_footprint: u64,
    ri_conclave_footprint: u64,
    ri_page_wait_time_mach: u64,
    ri_page_cache_hits: u64,
    ri_reserved: [u64; 6],
}

const RUSAGE_INFO_V4: c_int = 4;
const RUSAGE_INFO_V6: c_int = 6;

impl RusageInfoV6 {
    /// Cumulative energy of the process in nanojoules, as far as `flavor` can tell.
    ///
    /// V6 has the process's own CPU energy (`ri_energy_nj`). V4 only has
    /// `ri_billed_energy`, which no longer advances for a CPU-bound process on
    /// macOS 27, so it is used only where the kernel has nothing better.
    fn energy_nj(&self, flavor: c_int) -> u64 {
        if flavor == RUSAGE_INFO_V6 {
            self.ri_energy_nj
        } else {
            self.ri_billed_energy
        }
    }
}

/// Reads one process's counters. `None` for a process we may not inspect
/// (another user's: EPERM) or that has just exited (ESRCH).
fn read_rusage(pid: i32, flavor: c_int) -> Option<RusageInfoV6> {
    let mut info = RusageInfoV6::default();
    // SAFETY: `info` is a live, writable buffer the size of `rusage_info_v6`
    // (asserted in the tests below). The kernel writes exactly the struct of
    // the requested flavor, or nothing if it rejects the flavor, and we only
    // request V6 or V4, whose struct is a prefix of V6. Every field is a plain
    // integer, so any bytes are valid.
    let rc = unsafe { libc::proc_pid_rusage(pid, flavor, (&raw mut info).cast()) };
    (rc == 0).then_some(info)
}

/// The rusage flavor to ask this kernel for: V6 where it has it, else V4.
///
/// Decided once so that a process is never measured by one counter on one
/// scan and by a different counter on the next.
fn rusage_flavor() -> c_int {
    static FLAVOR: OnceLock<c_int> = OnceLock::new();
    *FLAVOR.get_or_init(|| {
        // Our own pid always exists and is always readable, so a failure here
        // can only mean that the kernel rejects the flavor (EINVAL).
        let own_pid = std::process::id() as i32;
        if read_rusage(own_pid, RUSAGE_INFO_V6).is_some() {
            RUSAGE_INFO_V6
        } else {
            RUSAGE_INFO_V4
        }
    })
}

fn process_name(pid: i32) -> String {
    // proc_name refuses buffers shorter than 2 * MAXCOMLEN (32 bytes).
    let mut buf = [0u8; 64];
    // SAFETY: `buf` is writable for the length we pass; proc_name copies at
    // most 32 bytes into it and returns the name's length (0 on error).
    let len = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let len = usize::try_from(len).unwrap_or(0).min(buf.len());
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

/// One process's counters from a single scan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessSample {
    pub name: String,
    /// Energy used since the process started, in nanojoules.
    pub energy_nj: u64,
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    pub phys_mem_bytes: u64,
}

/// Counters of every readable process, by pid.
pub type ProcessScan = HashMap<i32, ProcessSample>;

const MAX_PIDS: usize = 4096;

/// Scans all processes. Without root only the current user's processes are
/// readable; the rest are left out.
pub fn read_all_process_energy() -> ProcessScan {
    let flavor = rusage_flavor();
    let mut pids = [0i32; MAX_PIDS];
    // SAFETY: the buffer is writable for the byte size we pass; the call
    // returns how many pids it stored there.
    let stored = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(&pids) as c_int,
        )
    };
    let stored = usize::try_from(stored).unwrap_or(0).min(pids.len());

    let mut scan = ProcessScan::with_capacity(stored);
    for &pid in &pids[..stored] {
        if pid <= 0 {
            continue;
        }
        let Some(info) = read_rusage(pid, flavor) else {
            continue;
        };
        scan.insert(
            pid,
            ProcessSample {
                name: process_name(pid),
                energy_nj: info.energy_nj(flavor),
                disk_read_bytes: info.ri_diskio_bytesread,
                disk_write_bytes: info.ri_diskio_byteswritten,
                phys_mem_bytes: info.ri_phys_footprint,
            },
        );
    }
    scan
}

// ── Accounting ───────────────────────────────────────────────────────────────

/// Power is averaged over this much history.
const POWER_WINDOW: Duration = Duration::from_secs(5);
/// An exited process stays listed (as `alive: false`) for this long, so that
/// the energy it used does not vanish from view the moment it exits.
const KEEP_DEAD_FOR: Duration = Duration::from_secs(30);
/// Processes reported per update, picked by session energy.
const MAX_REPORTED: usize = 50;
/// Two readings closer together than this say nothing useful about power.
const MIN_SPAN_SECS: f64 = 0.01;

/// Readings of a cumulative energy counter covering the last [`POWER_WINDOW`].
///
/// The average power over the window is the energy difference between its two
/// ends divided by the time between them. That is exactly the time-weighted
/// mean of the per-tick powers, with nothing to average twice.
#[derive(Default)]
struct EnergyWindow {
    readings: VecDeque<(Instant, u64)>,
}

impl EnergyWindow {
    fn push(&mut self, now: Instant, energy_nj: u64) {
        self.readings.push_back((now, energy_nj));
        // The front may go once its successor is old enough to start the
        // window; until then it is needed to span the full window.
        while self
            .readings
            .get(1)
            .is_some_and(|&(t, _)| now.duration_since(t) >= POWER_WINDOW)
        {
            self.readings.pop_front();
        }
    }

    fn power_w(&self) -> f32 {
        let (Some(&(t0, e0)), Some(&(t1, e1))) = (self.readings.front(), self.readings.back())
        else {
            return 0.0;
        };
        let secs = t1.duration_since(t0).as_secs_f64();
        if secs < MIN_SPAN_SECS {
            return 0.0;
        }
        (e1.saturating_sub(e0) as f64 / 1e9 / secs) as f32
    }
}

/// What the tracker remembers about one process.
struct Tracked {
    name: String,
    /// Last kernel counter value, to form the next delta.
    energy_nj: u64,
    /// Energy used since the tracker first saw the process.
    session_nj: u64,
    window: EnergyWindow,
    last_seen: Instant,
    disk_read_bytes: u64,
    disk_write_bytes: u64,
    phys_mem_bytes: u64,
}

impl Tracked {
    /// Starts tracking at the current counter value: energy the process used
    /// before we first saw it is not part of this session.
    fn new(sample: &ProcessSample, now: Instant) -> Self {
        let mut window = EnergyWindow::default();
        window.push(now, 0);
        Self {
            name: sample.name.clone(),
            energy_nj: sample.energy_nj,
            session_nj: 0,
            window,
            last_seen: now,
            disk_read_bytes: sample.disk_read_bytes,
            disk_write_bytes: sample.disk_write_bytes,
            phys_mem_bytes: sample.phys_mem_bytes,
        }
    }
}

/// Result of one [`ProcessTracker::update`].
#[derive(Debug, Default)]
pub struct ProcessReport {
    /// Top processes by session energy, highest first. Network counters are
    /// left at zero; the tracker does not know them.
    pub processes: Vec<ProcessPower>,
    /// Power of all tracked processes together, averaged like each process's.
    pub total_power_w: f32,
    /// Energy of all tracked processes since the tracker started. Never
    /// decreases, also not when exited processes are forgotten.
    pub total_energy_mj: f64,
}

/// Turns successive process scans into per-process power and session energy.
#[derive(Default)]
pub struct ProcessTracker {
    known: HashMap<i32, Tracked>,
    /// Every delta ever attributed, summed. Kept apart from `known` so that
    /// forgetting a process cannot lower the total.
    session_nj: u64,
    window: EnergyWindow,
}

impl ProcessTracker {
    /// Accounts for one scan taken at `now`.
    pub fn update(&mut self, scan: &ProcessScan, now: Instant) -> ProcessReport {
        for (&pid, sample) in scan {
            let tracked = match self.known.entry(pid) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    entry.insert(Tracked::new(sample, now));
                    continue;
                }
            };
            // A different name or a counter that went backwards means the pid
            // now belongs to another process: start over instead of billing
            // the newcomer for the difference between two unrelated counters.
            if tracked.name != sample.name || sample.energy_nj < tracked.energy_nj {
                *tracked = Tracked::new(sample, now);
                continue;
            }
            let delta_nj = sample.energy_nj - tracked.energy_nj;
            tracked.energy_nj = sample.energy_nj;
            tracked.session_nj += delta_nj;
            tracked.window.push(now, tracked.session_nj);
            tracked.last_seen = now;
            tracked.disk_read_bytes = sample.disk_read_bytes;
            tracked.disk_write_bytes = sample.disk_write_bytes;
            tracked.phys_mem_bytes = sample.phys_mem_bytes;
            self.session_nj += delta_nj;
        }
        self.known
            .retain(|_, tracked| now.duration_since(tracked.last_seen) < KEEP_DEAD_FOR);
        self.window.push(now, self.session_nj);

        let mut top: Vec<(i32, &Tracked)> = self
            .known
            .iter()
            .filter(|(_, tracked)| tracked.session_nj > 0)
            .map(|(&pid, tracked)| (pid, tracked))
            .collect();
        // Ties broken by pid so the order does not depend on hash order.
        top.sort_unstable_by(|a, b| b.1.session_nj.cmp(&a.1.session_nj).then(a.0.cmp(&b.0)));
        top.truncate(MAX_REPORTED);

        let processes = top
            .into_iter()
            .map(|(pid, tracked)| {
                let alive = scan.contains_key(&pid);
                ProcessPower {
                    pid,
                    name: tracked.name.clone(),
                    // An exited process draws nothing. Its window describes
                    // the past and would otherwise repeat its last power for
                    // as long as the process stays listed.
                    power_w: if alive { tracked.window.power_w() } else { 0.0 },
                    energy_mj: tracked.session_nj as f64 / 1e6,
                    alive,
                    disk_read_bytes: tracked.disk_read_bytes,
                    disk_write_bytes: tracked.disk_write_bytes,
                    phys_mem_bytes: tracked.phys_mem_bytes,
                    net_rx_bytes: 0,
                    net_tx_bytes: 0,
                }
            })
            .collect();

        ProcessReport {
            processes,
            total_power_w: self.window.power_w(),
            total_energy_mj: self.session_nj as f64 / 1e6,
        }
    }
}

// ── Network counters ─────────────────────────────────────────────────────────

/// Per-process network counters cost a `nettop` subprocess per reading, far
/// more than the rest of a scan, so they are refreshed only this often and the
/// last reading is reused in between.
pub const NET_COUNTERS_REFRESH: Duration = Duration::from_secs(2);

/// How long the network counters of a process must stay unchanged before a
/// consumer may call it idle. Derived from the refresh period because a shorter
/// wait mistakes the pause before the next refresh for silence; the margin
/// covers a consumer that itself polls only every 2 s.
pub const NET_COUNTERS_IDLE_SECS: f64 = 2.5 * NET_COUNTERS_REFRESH.as_secs_f64();

/// Whether something last refreshed at `last` (`None`: never) is due at `now`.
pub fn refresh_due(last: Option<Instant>, now: Instant, period: Duration) -> bool {
    last.is_none_or(|last| now.duration_since(last) >= period)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    // Expected numbers come from compiling <sys/resource.h> (macOS 27 SDK)
    // with the C compiler: sizeof and offsetof of struct rusage_info_v6.
    #[test]
    fn rusage_info_v6_matches_the_c_layout() {
        assert_eq!(size_of::<RusageInfoV6>(), 464);
        assert_eq!(align_of::<RusageInfoV6>(), 8);
        assert_eq!(offset_of!(RusageInfoV6, ri_phys_footprint), 72);
        assert_eq!(offset_of!(RusageInfoV6, ri_diskio_bytesread), 144);
        assert_eq!(offset_of!(RusageInfoV6, ri_diskio_byteswritten), 152);
        assert_eq!(offset_of!(RusageInfoV6, ri_billed_energy), 264);
        // rusage_info_v4 ends where ri_flags begins.
        assert_eq!(offset_of!(RusageInfoV6, ri_flags), 296);
        assert_eq!(offset_of!(RusageInfoV6, ri_energy_nj), 336);
        assert_eq!(offset_of!(RusageInfoV6, ri_reserved), 416);
    }

    #[test]
    fn energy_comes_from_the_field_the_flavor_provides() {
        let info = RusageInfoV6 {
            ri_billed_energy: 3,
            ri_energy_nj: 7,
            ..Default::default()
        };
        assert_eq!(info.energy_nj(RUSAGE_INFO_V6), 7);
        assert_eq!(info.energy_nj(RUSAGE_INFO_V4), 3);
    }

    #[test]
    fn rejected_flavor_reads_as_none() {
        // rusage_flavor() relies on this to detect a kernel without V6. A
        // negative flavor can never become valid, so nothing is written.
        assert!(read_rusage(std::process::id() as i32, -1).is_none());
    }

    #[test]
    fn scan_includes_this_process() {
        let scan = read_all_process_energy();
        let own = scan
            .get(&(std::process::id() as i32))
            .expect("our own process must be readable");
        assert!(!own.name.is_empty());
        assert!(own.phys_mem_bytes > 0);
    }

    // ── ProcessTracker ───────────────────────────────────────────────────

    /// Nanojoules per joule.
    const J: u64 = 1_000_000_000;

    fn scan(processes: &[(i32, &str, u64)]) -> ProcessScan {
        processes
            .iter()
            .map(|&(pid, name, energy_nj)| {
                let sample = ProcessSample {
                    name: name.to_string(),
                    energy_nj,
                    ..Default::default()
                };
                (pid, sample)
            })
            .collect()
    }

    fn row(report: &ProcessReport, pid: i32) -> Option<&ProcessPower> {
        report.processes.iter().find(|p| p.pid == pid)
    }

    /// A clock for the tests: `at(3.0)` is three seconds after its start.
    fn clock() -> impl Fn(f64) -> Instant {
        let start = Instant::now();
        move |secs| start + Duration::from_secs_f64(secs)
    }

    #[test]
    fn first_sight_only_sets_the_baseline() {
        let at = clock();
        let mut tracker = ProcessTracker::default();
        // 50 J were used before we started watching; none of it is ours.
        let report = tracker.update(&scan(&[(1, "old-timer", 50 * J)]), at(0.0));
        assert!(report.processes.is_empty());
        assert_eq!(report.total_energy_mj, 0.0);
        assert_eq!(report.total_power_w, 0.0);
    }

    #[test]
    fn steady_process_reports_power_and_session_energy() {
        let at = clock();
        let mut tracker = ProcessTracker::default();
        tracker.update(&scan(&[(1, "busy", 50 * J)]), at(0.0));

        let report = tracker.update(&scan(&[(1, "busy", 52 * J)]), at(1.0));
        let busy = row(&report, 1).expect("listed once it has used energy");
        assert_eq!(busy.name, "busy");
        assert!(busy.alive);
        assert_eq!(busy.power_w, 2.0);
        assert_eq!(busy.energy_mj, 2000.0);
        assert_eq!(report.total_power_w, 2.0);
        assert_eq!(report.total_energy_mj, 2000.0);

        // 3 J over the 2 s seen so far.
        let report = tracker.update(&scan(&[(1, "busy", 53 * J)]), at(2.0));
        assert_eq!(row(&report, 1).unwrap().power_w, 1.5);
        assert_eq!(row(&report, 1).unwrap().energy_mj, 3000.0);
    }

    #[test]
    fn power_covers_one_window_and_no_more() {
        let at = clock();
        let mut tracker = ProcessTracker::default();
        // 1 W for ten seconds, then 3 W.
        let energy_at = |sec: u64| {
            if sec <= 10 {
                sec * J
            } else {
                10 * J + (sec - 10) * 3 * J
            }
        };
        let mut power_at = Vec::new();
        for sec in 0..=20 {
            let report = tracker.update(&scan(&[(1, "p", energy_at(sec))]), at(sec as f64));
            power_at.push(report.total_power_w);
            assert_eq!(
                row(&report, 1).map_or(0.0, |p| p.power_w),
                report.total_power_w,
                "a lone process is the whole total"
            );
        }
        assert_eq!(power_at[10], 1.0);
        // Two of the last five seconds at 3 W: (3 * 1 + 2 * 3) / 5.
        assert!((power_at[12] - 1.8).abs() < 1e-6, "got {}", power_at[12]);
        // One window after the step the old level is gone. Averaging the
        // total a second time would still be on its way up here.
        assert_eq!(power_at[15], 3.0);
        assert_eq!(power_at[20], 3.0);
    }

    #[test]
    fn renamed_pid_is_a_new_process() {
        let at = clock();
        let mut tracker = ProcessTracker::default();
        tracker.update(&scan(&[(7, "old", 10 * J)]), at(0.0));
        tracker.update(&scan(&[(7, "old", 12 * J)]), at(1.0));

        // The pid comes back under another name with an unrelated counter.
        let report = tracker.update(&scan(&[(7, "new", 40 * J)]), at(2.0));
        assert!(
            row(&report, 7).is_none(),
            "the newcomer has used nothing yet"
        );
        assert_eq!(
            report.total_energy_mj, 2000.0,
            "what `old` used stays counted"
        );

        let report = tracker.update(&scan(&[(7, "new", 41 * J)]), at(3.0));
        let new = row(&report, 7).unwrap();
        assert_eq!(new.name, "new");
        assert_eq!(new.energy_mj, 1000.0);
        assert_eq!(new.power_w, 1.0);
        assert_eq!(report.total_energy_mj, 3000.0);
    }

    #[test]
    fn counter_going_backwards_is_a_new_process() {
        let at = clock();
        let mut tracker = ProcessTracker::default();
        tracker.update(&scan(&[(7, "p", 10 * J)]), at(0.0));
        tracker.update(&scan(&[(7, "p", 12 * J)]), at(1.0));

        let report = tracker.update(&scan(&[(7, "p", J)]), at(2.0));
        assert!(row(&report, 7).is_none());
        assert_eq!(report.total_energy_mj, 2000.0);

        let report = tracker.update(&scan(&[(7, "p", J + J / 2)]), at(3.0));
        assert_eq!(row(&report, 7).unwrap().energy_mj, 500.0);
        assert_eq!(report.total_energy_mj, 2500.0);
    }

    #[test]
    fn dead_process_draws_nothing_but_keeps_its_energy() {
        let at = clock();
        let mut tracker = ProcessTracker::default();
        tracker.update(&scan(&[(1, "dies", 10 * J), (2, "lives", 20 * J)]), at(0.0));
        tracker.update(&scan(&[(1, "dies", 12 * J), (2, "lives", 21 * J)]), at(1.0));

        // Gone from the very next scan: zero power at once, not a repeat of
        // the 2 W it drew while it lived.
        let report = tracker.update(&scan(&[(2, "lives", 22 * J)]), at(2.0));
        let dead = row(&report, 1).expect("stays listed for a while");
        assert!(!dead.alive);
        assert_eq!(dead.power_w, 0.0);
        assert_eq!(dead.energy_mj, 2000.0);
        assert_eq!(row(&report, 2).unwrap().power_w, 1.0);
        assert_eq!(report.total_energy_mj, 4000.0);

        // Once the energy it really used has left the window, the total is
        // the survivor's power alone.
        let mut report = report;
        for sec in 3..=8 {
            report = tracker.update(&scan(&[(2, "lives", (20 + sec) * J)]), at(sec as f64));
            assert_eq!(row(&report, 1).unwrap().power_w, 0.0);
            assert_eq!(row(&report, 1).unwrap().energy_mj, 2000.0);
        }
        assert_eq!(report.total_power_w, 1.0);
    }

    #[test]
    fn forgetting_a_dead_process_does_not_lower_the_total() {
        let at = clock();
        let mut tracker = ProcessTracker::default();
        tracker.update(&scan(&[(1, "dies", 10 * J), (2, "lives", 20 * J)]), at(0.0));
        tracker.update(&scan(&[(1, "dies", 15 * J), (2, "lives", 20 * J)]), at(1.0));

        let mut previous_total = 0.0;
        let mut listed_at = Vec::new();
        for sec in 2..=40 {
            let report = tracker.update(&scan(&[(2, "lives", 20 * J)]), at(sec as f64));
            assert!(report.total_energy_mj >= previous_total);
            previous_total = report.total_energy_mj;
            listed_at.push((sec, row(&report, 1).is_some()));
        }
        assert_eq!(previous_total, 5000.0);
        // Last seen at 1 s, so listed until 30 s later and not beyond.
        assert!(listed_at.contains(&(30, true)));
        assert!(listed_at.contains(&(31, false)));
    }

    #[test]
    fn process_missing_from_one_scan_resumes_without_losing_energy() {
        let at = clock();
        let mut tracker = ProcessTracker::default();
        tracker.update(&scan(&[(1, "p", 10 * J)]), at(0.0));
        tracker.update(&scan(&[(1, "p", 11 * J)]), at(1.0));
        let report = tracker.update(&scan(&[]), at(2.0));
        assert!(!row(&report, 1).unwrap().alive);

        let report = tracker.update(&scan(&[(1, "p", 13 * J)]), at(3.0));
        let back = row(&report, 1).unwrap();
        assert!(back.alive);
        assert_eq!(back.energy_mj, 3000.0);
        assert_eq!(back.power_w, 1.0);
    }

    #[test]
    fn empty_scan_reports_positive_zero() {
        let at = clock();
        let mut tracker = ProcessTracker::default();
        for sec in 0..3 {
            let report = tracker.update(&ProcessScan::new(), at(sec as f64));
            assert!(report.processes.is_empty());
            // Bit patterns, because -0.0 == 0.0 but prints as "-0.0".
            assert_eq!(report.total_power_w.to_bits(), 0.0f32.to_bits());
            assert_eq!(report.total_energy_mj.to_bits(), 0.0f64.to_bits());
        }
    }

    #[test]
    fn reports_the_top_processes_by_session_energy() {
        let at = clock();
        let mut tracker = ProcessTracker::default();
        let pids = 1..=(MAX_REPORTED as i32 + 10);
        let before: Vec<_> = pids.clone().map(|pid| (pid, "p", 0)).collect();
        // Higher pids use more; pids 1 and 2 tie.
        let after: Vec<_> = pids
            .clone()
            .map(|pid| (pid, "p", pid.max(2) as u64 * J))
            .collect();
        tracker.update(&scan(&before), at(0.0));
        let report = tracker.update(&scan(&after), at(1.0));

        assert_eq!(report.processes.len(), MAX_REPORTED);
        let listed: Vec<i32> = report.processes.iter().map(|p| p.pid).collect();
        let expected: Vec<i32> = pids.clone().rev().take(MAX_REPORTED).collect();
        assert_eq!(listed, expected);
        // The total covers every process, listed or not.
        let all_joules: u64 = pids.map(|pid| pid.max(2) as u64).sum();
        assert_eq!(report.total_energy_mj, all_joules as f64 * 1000.0);
    }

    #[test]
    fn refresh_is_due_at_first_and_then_once_per_period() {
        let at = clock();
        let period = Duration::from_secs(2);
        assert!(refresh_due(None, at(0.0), period));
        assert!(!refresh_due(Some(at(0.0)), at(0.25), period));
        assert!(!refresh_due(Some(at(0.0)), at(1.99), period));
        assert!(refresh_due(Some(at(0.0)), at(2.0), period));

        // The sampler's loop at its default 250 ms tick, for 12 s.
        let mut last = None;
        let mut refreshes = 0;
        for tick in 0..48 {
            let now = at(tick as f64 * 0.25);
            if refresh_due(last, now, period) {
                last = Some(now);
                refreshes += 1;
            }
        }
        assert_eq!(refreshes, 6, "not once per tick (48)");
    }

    #[test]
    fn equal_energies_are_listed_in_pid_order() {
        let at = clock();
        let mut tracker = ProcessTracker::default();
        tracker.update(&scan(&[(9, "a", 0), (3, "b", 0), (5, "c", 0)]), at(0.0));
        let report = tracker.update(&scan(&[(9, "a", J), (3, "b", J), (5, "c", J)]), at(1.0));
        let listed: Vec<i32> = report.processes.iter().map(|p| p.pid).collect();
        assert_eq!(listed, [3, 5, 9]);
    }
}
