//! Per-process power.
//!
//! The kernel keeps a cumulative energy counter for every process
//! (`proc_pid_rusage`). [`read_all_process_energy`] reads those counters.

use libc::c_int;
use std::collections::HashMap;
use std::sync::OnceLock;

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
}
