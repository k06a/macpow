use crate::cf_utils;
use crate::iokit_ffi::*;
use crate::process_utils::command_output_timeout;
use crate::types::{BluetoothDevice, EthernetInfo, PowerAssertion, UsbDevice, WifiInfo};
use core_foundation_sys::array::CFArrayRef;
use core_foundation_sys::dictionary::{CFDictionaryRef, CFMutableDictionaryRef};
use core_foundation_sys::number::CFNumberRef;

// ── USB / Thunderbolt enumeration ────────────────────────────────────────────

pub fn list_usb_devices() -> Vec<UsbDevice> {
    unsafe { list_usb_inner().unwrap_or_default() }
}

unsafe fn list_usb_inner() -> Option<Vec<UsbDevice>> {
    let matching = IOServiceMatching(b"IOUSBHostDevice\0".as_ptr() as *const i8);
    if matching.is_null() {
        return None;
    }

    let mut iter: u32 = 0;
    if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 {
        return None;
    }

    // Collect all entry handles first to avoid iterator invalidation
    // from find_storage_stats child-tree walks.
    let mut entries = Vec::new();
    loop {
        let entry = IOIteratorNext(iter);
        if entry == 0 {
            break;
        }
        entries.push(entry);
    }
    IOObjectRelease(iter);

    let mut devices = Vec::new();
    for entry in &entries {
        let entry = *entry;

        let mut name_buf = [0i8; 128];
        let name = if IORegistryEntryGetName(entry, name_buf.as_mut_ptr()) == 0 {
            std::ffi::CStr::from_ptr(name_buf.as_ptr())
                .to_string_lossy()
                .into_owned()
        } else {
            "Unknown".into()
        };

        let mut props: CFMutableDictionaryRef = std::ptr::null_mut();
        let mut vendor_id: u32 = 0;
        let mut product_id: u32 = 0;
        let mut power_ma: Option<u32> = None;
        let mut speed: u32 = 0;
        let mut location_id: u32 = 0;

        if IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null(), 0) == 0
            && !props.is_null()
        {
            let dict = props as CFDictionaryRef;
            vendor_id = cf_utils::cfdict_get_i64(dict, "idVendor").unwrap_or(0) as u32;
            product_id = cf_utils::cfdict_get_i64(dict, "idProduct").unwrap_or(0) as u32;
            power_ma = cf_utils::cfdict_get_i64(dict, "UsbPowerSinkAllocation")
                .or_else(|| cf_utils::cfdict_get_i64(dict, "USB Power"))
                .or_else(|| cf_utils::cfdict_get_i64(dict, "bMaxPower"))
                .map(|v| v as u32);
            speed = cf_utils::cfdict_get_i64(dict, "Device Speed").unwrap_or(0) as u32;
            location_id = cf_utils::cfdict_get_i64(dict, "locationID").unwrap_or(0) as u32;
            cf_utils::cf_release(props as _);
        }

        let (bytes_read, bytes_written) = find_storage_stats(entry);

        IOObjectRelease(entry);

        devices.push(UsbDevice {
            name,
            vendor_id,
            product_id,
            power_ma,
            speed,
            location_id,
            bytes_read,
            bytes_written,
            parent_location_id: 0,
        });
    }

    compute_usb_parents(&mut devices);
    Some(devices)
}

/// Compute parent-child relationships from USB locationID topology.
/// locationID encodes the port path: upper nibbles are parent ports,
/// each additional non-zero nibble adds a hub level.
/// E.g. 0x01100000 (hub) is parent of 0x01110000 and 0x01120000.
fn compute_usb_parents(devices: &mut [UsbDevice]) {
    let locs: Vec<u32> = devices.iter().map(|d| d.location_id).collect();
    for i in 0..devices.len() {
        let loc = devices[i].location_id;
        if loc == 0 {
            continue;
        }
        // Scan nibbles from bottom (shift=0) upward to find the lowest non-zero
        // nibble in the port path, then clear it to get the parent locationID.
        // E.g. 0x01110000 → lowest non-zero nibble at shift=16 → parent 0x01100000
        let mut shift = 0u32;
        while shift <= 28 {
            if (loc >> shift) & 0xF != 0 {
                break;
            }
            shift += 4;
        }
        if shift <= 24 {
            let parent_loc = loc & !(0xF << shift);
            if parent_loc != 0 && locs.contains(&parent_loc) {
                devices[i].parent_location_id = parent_loc;
            }
        }
    }
}

/// Walk child tree of a USB device to find IOBlockStorageDriver Statistics.
unsafe fn find_storage_stats(entry: u32) -> (u64, u64) {
    const KIO_REGISTRY_ITERATE_RECURSIVELY: u32 = 1;
    let mut child_iter: u32 = 0;
    let plane = b"IOService\0".as_ptr() as *const i8;
    if IORegistryEntryCreateIterator(
        entry,
        plane,
        KIO_REGISTRY_ITERATE_RECURSIVELY,
        &mut child_iter,
    ) != 0
    {
        return (0, 0);
    }
    let mut result = (0u64, 0u64);
    loop {
        let child = IOIteratorNext(child_iter);
        if child == 0 {
            break;
        }
        let mut props: CFMutableDictionaryRef = std::ptr::null_mut();
        if IORegistryEntryCreateCFProperties(child, &mut props, std::ptr::null(), 0) == 0
            && !props.is_null()
        {
            let dict = props as CFDictionaryRef;
            let stats = cf_utils::cfdict_get(dict, "Statistics");
            if !stats.is_null() {
                let stats_dict = stats as CFDictionaryRef;
                let br = cf_utils::cfdict_get_i64(stats_dict, "Bytes (Read)").unwrap_or(0) as u64;
                let bw = cf_utils::cfdict_get_i64(stats_dict, "Bytes (Write)").unwrap_or(0) as u64;
                if br > 0 || bw > 0 {
                    result = (br, bw);
                }
            }
            cf_utils::cf_release(props as _);
        }
        IOObjectRelease(child);
        if result.0 > 0 || result.1 > 0 {
            break;
        }
    }
    IOObjectRelease(child_iter);
    result
}

// ── Power assertions ─────────────────────────────────────────────────────────

pub fn list_power_assertions() -> Vec<PowerAssertion> {
    unsafe { list_assertions_inner().unwrap_or_default() }
}

unsafe fn list_assertions_inner() -> Option<Vec<PowerAssertion>> {
    use core_foundation::base::TCFType;
    use core_foundation::number::CFNumber;

    let mut dict: CFDictionaryRef = std::ptr::null();
    let kr = IOPMCopyAssertionsByProcess(&mut dict);
    if kr != 0 || dict.is_null() {
        return None;
    }

    let mut result = Vec::new();

    // dict: { PID (CFNumber) -> CFArray of assertion dicts }
    // We iterate using CFDictionaryGetKeysAndValues
    let count = core_foundation_sys::dictionary::CFDictionaryGetCount(dict);
    if count == 0 {
        cf_utils::cf_release(dict as _);
        return Some(result);
    }

    let mut keys = vec![std::ptr::null(); count as usize];
    let mut vals = vec![std::ptr::null(); count as usize];
    core_foundation_sys::dictionary::CFDictionaryGetKeysAndValues(
        dict,
        keys.as_mut_ptr(),
        vals.as_mut_ptr(),
    );

    for i in 0..count as usize {
        let pid_ref = keys[i] as CFNumberRef;
        let pid_cf: CFNumber = TCFType::wrap_under_get_rule(pid_ref);
        let pid = pid_cf.to_i64().unwrap_or(0);

        let arr = vals[i] as CFArrayRef;
        let arr_len = cf_utils::cfarray_len(arr);

        for j in 0..arr_len {
            let entry = cf_utils::cfarray_get(arr, j) as CFDictionaryRef;
            let atype = cf_utils::cfdict_get_string(entry, "AssertionType")
                .or_else(|| cf_utils::cfdict_get_string(entry, "AssertType"))
                .unwrap_or_default();
            let aname = cf_utils::cfdict_get_string(entry, "AssertName").unwrap_or_default();

            result.push(PowerAssertion {
                name: aname,
                assertion_type: atype,
                pid,
            });
        }
    }

    cf_utils::cf_release(dict as _);
    Some(result)
}

// ── WiFi info via CoreWLAN (ObjC runtime) + ipconfig (SSID fallback) ─────────

#[link(name = "CoreWLAN", kind = "framework")]
extern "C" {}

extern "C" {
    fn objc_getClass(name: *const libc::c_char) -> *mut libc::c_void;
    fn sel_registerName(name: *const libc::c_char) -> *mut libc::c_void;
    // Declared without a signature on purpose, as in <objc/message.h>:
    // objc_msgSend jumps straight into the method, so every call has to go
    // through a pointer typed like that method (`send_obj` / `send_int` /
    // `send_f64` below), never through a variadic prototype.
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut libc::c_void;
    fn objc_autoreleasePoolPop(pool: *mut libc::c_void);
}

/// `objc_msgSend` typed as a method that takes no arguments and returns `R`.
///
/// The caller's prototype decides which register the result is read from (x0
/// for pointers and integers, d0 for doubles), so `R` has to be the method's
/// real return type. Reading a `double` through a pointer-returning prototype
/// is how `transmitRate` used to come back as NaN.
type MsgSend<R> = unsafe extern "C" fn(*mut libc::c_void, *mut libc::c_void) -> R;

// Safety contract shared by the three helpers below: `obj` is a live object
// or class (or nil, which answers zero), and the method named by `sel` takes
// no arguments and returns exactly the helper's return type.

/// Sends a message whose method returns an object pointer.
unsafe fn send_obj(obj: *mut libc::c_void, sel: &std::ffi::CStr) -> *mut libc::c_void {
    // SAFETY: only the pointer's type changes; per the contract above it now
    // matches the method that objc_msgSend dispatches to.
    let send: MsgSend<*mut libc::c_void> =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    send(obj, sel_registerName(sel.as_ptr()))
}

/// Sends a message whose method returns an `NSInteger` (or an `NSInteger` enum).
unsafe fn send_int(obj: *mut libc::c_void, sel: &std::ffi::CStr) -> isize {
    // SAFETY: as in `send_obj`.
    let send: MsgSend<isize> = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    send(obj, sel_registerName(sel.as_ptr()))
}

/// Sends a message whose method returns a `double`.
unsafe fn send_f64(obj: *mut libc::c_void, sel: &std::ffi::CStr) -> f64 {
    // SAFETY: as in `send_obj`.
    let send: MsgSend<f64> = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    send(obj, sel_registerName(sel.as_ptr()))
}

/// Releases, when dropped, the Objective-C objects that were autoreleased
/// while it was alive.
///
/// The sampler threads are plain Rust threads: nothing drains autoreleased
/// objects for them, so without a pool every CoreWLAN call leaks its
/// temporaries until the process exits.
///
/// Keep it a scoped local. Pools have to be popped in the reverse order of
/// their creation, on the thread that pushed them; locals are dropped in
/// reverse declaration order, and the raw pointer keeps the guard `!Send`.
struct AutoreleasePool(*mut libc::c_void);

impl AutoreleasePool {
    fn new() -> Self {
        // SAFETY: pushing a pool has no preconditions; the token goes back
        // to the runtime exactly once, in `drop`.
        Self(unsafe { objc_autoreleasePoolPush() })
    }
}

impl Drop for AutoreleasePool {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the token of a pool pushed on this thread and
        // not popped yet (see the type's doc comment).
        unsafe { objc_autoreleasePoolPop(self.0) }
    }
}

// ── Link carrier via SIOCGIFMEDIA ────────────────────────────────────────────

/// `struct ifmediareq` from <net/if_media.h>, which is laid out under
/// `#pragma pack(4)`. Only `name` (in) and `status` (out) are used here.
#[repr(C, packed(4))]
struct IfMediaReq {
    name: [libc::c_char; libc::IFNAMSIZ],
    _current: libc::c_int,
    _mask: libc::c_int,
    status: libc::c_int,
    _active: libc::c_int,
    _count: libc::c_int,
    _ulist: *mut libc::c_int,
}

/// `_IOWR('i', 56, struct ifmediareq)`.
const SIOCGIFMEDIA: libc::c_ulong = 0xc02c_6938;

// The request code carries the struct size and the kernel copies exactly that
// many bytes in and out, so the layout above has to agree with it.
const _: () =
    assert!(std::mem::size_of::<IfMediaReq>() == ((SIOCGIFMEDIA >> 16) & 0x1fff) as usize);

/// The driver filled in the status word.
const IFM_AVALID: libc::c_int = 0x1;
/// The link has carrier.
const IFM_ACTIVE: libc::c_int = 0x2;

/// What `ifconfig` prints as "status: active".
fn media_status_is_active(status: libc::c_int) -> bool {
    status & IFM_AVALID != 0 && status & IFM_ACTIVE != 0
}

/// True when `iface` has carrier, i.e. a peer on the other end of the link.
///
/// IFF_UP | IFF_RUNNING does not say that: idle Thunderbolt and USB-C (NCM)
/// ports keep both flags set with nothing plugged in.
fn link_is_active(iface: &str) -> bool {
    let mut req = IfMediaReq {
        name: [0; libc::IFNAMSIZ],
        _current: 0,
        _mask: 0,
        status: 0,
        _active: 0,
        _count: 0,
        _ulist: std::ptr::null_mut(),
    };
    // The kernel expects a NUL-terminated name, so one byte must stay zero.
    if iface.is_empty() || iface.len() >= req.name.len() {
        return false;
    }
    for (dst, src) in req.name.iter_mut().zip(iface.bytes()) {
        *dst = src as libc::c_char;
    }

    // SAFETY: `req` is a writable `ifmediareq` of the size encoded in the
    // request code, and with `_count == 0` the kernel does not follow
    // `_ulist`. The socket only gives the ioctl something to be issued on;
    // it is closed on every path that opened it.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return false;
        }
        let answered = libc::ioctl(fd, SIOCGIFMEDIA, &mut req as *mut IfMediaReq) == 0;
        libc::close(fd);
        answered && media_status_is_active(req.status)
    }
}

// ── Ethernet detection via getifaddrs ─────────────────────────────────────────

pub fn read_ethernet_info(wifi_iface: &str) -> EthernetInfo {
    #[repr(C)]
    struct IfData {
        ifi_type: u8,
        _pad: [u8; 7],
        ifi_mtu: u32,
        ifi_metric: u32,
        ifi_baudrate: u32,
    }

    const IFT_ETHER: u8 = 6;

    unsafe {
        let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut addrs) != 0 {
            return EthernetInfo::default();
        }

        let mut best: Option<(String, u32)> = None;
        let mut cur = addrs;
        while !cur.is_null() {
            let entry = &*cur;
            cur = entry.ifa_next;

            if entry.ifa_addr.is_null() || entry.ifa_data.is_null() {
                continue;
            }
            if (*entry.ifa_addr).sa_family as i32 != libc::AF_LINK {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(entry.ifa_name).to_string_lossy();
            if !name.starts_with("en") {
                continue;
            }
            // WiFi interfaces also report IFT_ETHER at link layer; skip the
            // known WiFi interface so we only detect real Ethernet ports.
            if !wifi_iface.is_empty() && name == wifi_iface {
                continue;
            }
            let data = &*(entry.ifa_data as *const IfData);
            if data.ifi_type != IFT_ETHER {
                continue;
            }
            let flags = entry.ifa_flags;
            let up = (flags & libc::IFF_UP as u32) != 0;
            let running = (flags & libc::IFF_RUNNING as u32) != 0;
            if !up || !running {
                continue;
            }
            // UP and RUNNING are set on idle ports too; only carrier means a
            // link. Asked last because it costs a socket and an ioctl.
            if !link_is_active(&name) {
                continue;
            }
            let speed_mbps = data.ifi_baudrate / 1_000_000;
            if best.is_none() || speed_mbps > best.as_ref().unwrap().1 {
                best = Some((name.into_owned(), speed_mbps));
            }
        }
        libc::freeifaddrs(addrs);

        match best {
            Some((iface, speed)) => EthernetInfo {
                connected: true,
                interface_name: iface,
                link_speed_mbps: speed,
            },
            None => EthernetInfo::default(),
        }
    }
}

// ── WiFi via CoreWLAN ─────────────────────────────────────────────────────────

fn phy_mode_str(mode: i64) -> &'static str {
    match mode {
        1 => "802.11a",
        2 => "802.11b",
        3 => "802.11g",
        4 => "802.11n",
        5 => "802.11ac",
        6 => "802.11ax",
        7 => "802.11be",
        _ => "",
    }
}

/// The network name in the output of `ipconfig getsummary`, if it has one.
fn parse_ipconfig_ssid(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(ssid) = trimmed.strip_prefix("SSID : ") {
            let s = ssid.trim().to_string();
            // macOS prints this placeholder instead of the name to callers
            // without Location permission. It means "unknown".
            if !s.is_empty() && s != "<redacted>" {
                return Some(s);
            }
        }
    }
    None
}

fn read_wifi_ssid_ipconfig(iface_name: &str) -> Option<String> {
    // Absolute path, so the answer does not depend on the caller's PATH.
    let output = command_output_timeout(
        "/usr/sbin/ipconfig",
        &["getsummary", iface_name],
        std::time::Duration::from_millis(1000),
    )?;
    parse_ipconfig_ssid(&String::from_utf8_lossy(&output.stdout))
}

/// How long one `ipconfig` answer is reused. The SSID only goes into the JSON
/// output, so a stale name for half a minute after a silent roam is harmless,
/// while asking on every tick meant about four process spawns a second.
const SSID_REFRESH: std::time::Duration = std::time::Duration::from_secs(30);

/// Wi-Fi source for the periodic sampler. It holds the one thing the reads
/// have to remember between ticks: the last `ipconfig` SSID answer.
#[derive(Default)]
pub struct WifiReader {
    /// When `ipconfig` was last asked and what it answered ("" = unknown).
    ssid_lookup: Option<(std::time::Instant, String)>,
}

impl WifiReader {
    /// The current Wi-Fi state.
    pub fn read(&mut self) -> WifiInfo {
        let mut info = read_wifi_corewlan();
        self.fill_ssid(
            &mut info,
            std::time::Instant::now(),
            read_wifi_ssid_ipconfig,
        );
        info
    }

    /// Completes `info.ssid` when CoreWLAN withheld it, running `lookup` at
    /// most once per `SSID_REFRESH` for as long as the link stays up.
    fn fill_ssid(
        &mut self,
        info: &mut WifiInfo,
        now: std::time::Instant,
        lookup: impl FnOnce(&str) -> Option<String>,
    ) {
        if !info.connected {
            // The next association may be with another network: ask again then.
            self.ssid_lookup = None;
        } else if info.ssid.is_empty() {
            match &self.ssid_lookup {
                Some((asked, answer)) if now.duration_since(*asked) < SSID_REFRESH => {
                    info.ssid = answer.clone();
                }
                _ => {
                    info.ssid = lookup(&info.interface_name).unwrap_or_default();
                    self.ssid_lookup = Some((now, info.ssid.clone()));
                }
            }
        }
    }
}

/// One-off Wi-Fi snapshot. A periodic caller should keep a `WifiReader`
/// instead, so that the SSID fallback is not run again on every call.
pub fn read_wifi_info() -> WifiInfo {
    WifiReader::default().read()
}

/// What CoreWLAN and the link state say. `ssid` stays empty when macOS
/// withholds it.
fn read_wifi_corewlan() -> WifiInfo {
    // Declared first, so it is dropped last: by then everything CoreWLAN
    // returned has been copied into the owned `WifiInfo` that is handed back.
    let _pool = AutoreleasePool::new();

    // SAFETY: every message goes to a class or object that was just obtained
    // and checked for nil, through the helper matching the return type the
    // CoreWLAN headers declare for that method (object, NSInteger or double).
    unsafe {
        let no_hardware = WifiInfo {
            phy_mode: "none".into(),
            ..Default::default()
        };

        let cls = objc_getClass(c"CWWiFiClient".as_ptr());
        if cls.is_null() {
            return no_hardware;
        }
        let client = send_obj(cls, c"sharedWiFiClient");
        if client.is_null() {
            return no_hardware;
        }
        let iface = send_obj(client, c"interface");
        if iface.is_null() {
            return no_hardware;
        }

        // Get the actual interface name (e.g. en0 on laptops, en1 on Mac Studio).
        let iface_name_ns = send_obj(iface, c"interfaceName");
        let iface_name = if !iface_name_ns.is_null() {
            cf_utils::cfstring_to_string(iface_name_ns as _).unwrap_or_default()
        } else {
            String::new()
        };

        // Associated or not is a property of the link, not of the name: the
        // SSID is withheld without Location permission while the link is up.
        if !link_is_active(&iface_name) {
            return WifiInfo {
                interface_name: iface_name,
                ..Default::default()
            };
        }

        // ssid requires location permission (nil for unsigned binaries since
        // Sonoma); `WifiReader` then falls back to ipconfig getsummary.
        let ssid_ns = send_obj(iface, c"ssid");
        let ssid = cf_utils::cfstring_to_string(ssid_ns as _).unwrap_or_default();

        let mut info = WifiInfo {
            connected: true,
            interface_name: iface_name,
            ssid,
            ..Default::default()
        };

        info.rssi_dbm = send_int(iface, c"rssiValue") as i32;
        info.noise_dbm = send_int(iface, c"noiseMeasurement") as i32;
        info.tx_rate_mbps = send_f64(iface, c"transmitRate") as f32;
        info.phy_mode = phy_mode_str(send_int(iface, c"activePHYMode") as i64).to_string();

        let channel = send_obj(iface, c"wlanChannel");
        if !channel.is_null() {
            let ch_num = send_int(channel, c"channelNumber");
            let ch_band = send_int(channel, c"channelBand");
            let band_str = match ch_band {
                1 => "2GHz",
                2 => "5GHz",
                3 => "6GHz",
                _ => "",
            };
            if !band_str.is_empty() {
                info.channel = format!("{} ({})", ch_num, band_str);
            } else {
                info.channel = format!("{}", ch_num);
            }
        }

        // WiFi power estimate: ~0.1W idle, up to ~0.8W active, scales with signal strength
        let signal_quality = ((info.rssi_dbm + 100).max(0).min(60)) as f32 / 60.0;
        info.estimated_power_w = 0.1 + (1.0 - signal_quality) * 0.7;

        info
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    #[test]
    fn link_is_active_only_with_valid_status_and_carrier() {
        // 0x1 is what the idle Thunderbolt / USB-C NCM ports report: the
        // status is valid and says "no carrier".
        assert!(!media_status_is_active(IFM_AVALID));
        assert!(media_status_is_active(IFM_AVALID | IFM_ACTIVE));
        // Without IFM_AVALID the driver did not fill in the other bits.
        assert!(!media_status_is_active(IFM_ACTIVE));
        assert!(!media_status_is_active(0));
        // Unrelated status bits do not change the answer.
        assert!(media_status_is_active(0x70 | IFM_AVALID | IFM_ACTIVE));
        assert!(!media_status_is_active(0x70 | IFM_AVALID));
    }

    #[test]
    fn link_is_active_rejects_names_the_kernel_cannot_take() {
        assert!(!link_is_active(""));
        // IFNAMSIZ bytes leave no room for the terminating NUL.
        assert!(!link_is_active(&"e".repeat(libc::IFNAMSIZ)));
        assert!(!link_is_active("no-such-if0"));
    }

    #[test]
    fn autorelease_pool_releases_its_objects_on_drop() {
        extern "C" {
            fn objc_storeWeak(
                location: *mut *mut libc::c_void,
                obj: *mut libc::c_void,
            ) -> *mut libc::c_void;
            fn objc_loadWeak(location: *mut *mut libc::c_void) -> *mut libc::c_void;
            fn objc_destroyWeak(location: *mut *mut libc::c_void);
        }

        // SAFETY: NSObject's alloc, init and autorelease all return an
        // object pointer, and `weak` is a live weak-reference slot from
        // objc_storeWeak until objc_destroyWeak.
        unsafe {
            let mut weak: *mut libc::c_void = std::ptr::null_mut();
            {
                let _pool = AutoreleasePool::new();
                let class = objc_getClass(c"NSObject".as_ptr());
                let obj = send_obj(send_obj(class, c"alloc"), c"init");
                objc_storeWeak(&mut weak, obj);
                // Hand the only strong reference over to the pool.
                send_obj(obj, c"autorelease");
                assert!(!objc_loadWeak(&mut weak).is_null());
            }
            let outlived_the_pool = !objc_loadWeak(&mut weak).is_null();
            objc_destroyWeak(&mut weak);
            assert!(!outlived_the_pool);
        }
    }

    /// `ipconfig getsummary` output, cut down to its top-level Wi-Fi keys.
    fn ipconfig_summary(bssid: &str, ssid: &str) -> String {
        format!(
            "<dictionary> {{\n  BSSID : {bssid}\n  InterfaceType : WiFi\n  \
             LinkStatusActive : TRUE\n  SSID : {ssid}\n  Security : WPA3_SAE\n}}\n"
        )
    }

    #[test]
    fn ipconfig_ssid_is_parsed() {
        let summary = ipconfig_summary("11:22:33:44:55:66", "Home Network");
        assert_eq!(
            parse_ipconfig_ssid(&summary).as_deref(),
            Some("Home Network")
        );
        assert_eq!(parse_ipconfig_ssid(&ipconfig_summary("", "")), None);
        assert_eq!(parse_ipconfig_ssid("  InterfaceType : WiFi\n"), None);
    }

    #[test]
    fn redacted_ipconfig_ssid_is_unknown() {
        // What macOS 27 answers to a caller without Location permission.
        let summary = ipconfig_summary("<redacted>", "<redacted>");
        assert_eq!(parse_ipconfig_ssid(&summary), None);
    }

    fn connected_wifi(ssid: &str) -> WifiInfo {
        WifiInfo {
            connected: true,
            interface_name: "en0".into(),
            ssid: ssid.into(),
            ..Default::default()
        }
    }

    #[test]
    fn ssid_lookup_runs_once_per_refresh_period() {
        let mut reader = WifiReader::default();
        let start = std::time::Instant::now();
        let mut lookups = 0;
        // One minute of sampling at the default 250 ms interval.
        for tick in 0..240 {
            let mut info = connected_wifi("");
            let now = start + std::time::Duration::from_millis(250 * tick);
            reader.fill_ssid(&mut info, now, |iface| {
                assert_eq!(iface, "en0");
                lookups += 1;
                Some("Home Network".into())
            });
            assert_eq!(info.ssid, "Home Network", "tick {tick}");
        }
        // At 0 s and at 30 s; it used to be once per tick.
        assert_eq!(lookups, 2);
    }

    #[test]
    fn unknown_ssid_is_remembered_too() {
        let mut reader = WifiReader::default();
        let start = std::time::Instant::now();
        let mut lookups = 0;
        for tick in 0..100 {
            let mut info = connected_wifi("");
            let now = start + std::time::Duration::from_millis(250 * tick);
            reader.fill_ssid(&mut info, now, |_| {
                lookups += 1;
                None
            });
            // No name is not "disconnected".
            assert!(info.connected);
            assert_eq!(info.ssid, "");
        }
        assert_eq!(lookups, 1);
    }

    #[test]
    fn ssid_is_looked_up_again_after_the_link_drops() {
        let mut reader = WifiReader::default();
        let now = std::time::Instant::now();

        let mut info = connected_wifi("");
        reader.fill_ssid(&mut info, now, |_| Some("Home Network".into()));
        assert_eq!(info.ssid, "Home Network");

        let mut down = WifiInfo::default();
        reader.fill_ssid(&mut down, now, |_| panic!("no lookup without a link"));
        assert_eq!(down.ssid, "");

        // Well inside the refresh period, but it may be another network now.
        let mut info = connected_wifi("");
        reader.fill_ssid(&mut info, now, |_| Some("Office".into()));
        assert_eq!(info.ssid, "Office");
    }

    #[test]
    fn ssid_from_corewlan_needs_no_lookup() {
        let mut reader = WifiReader::default();
        let mut info = connected_wifi("Home Network");
        reader.fill_ssid(&mut info, std::time::Instant::now(), |_| {
            panic!("CoreWLAN already answered")
        });
        assert_eq!(info.ssid, "Home Network");
    }
}

// ── Bluetooth devices via pmset -g accps ─────────────────────────────────────

pub fn read_bluetooth_devices() -> Vec<BluetoothDevice> {
    let output = match command_output_timeout(
        "pmset",
        &["-g", "accps"],
        std::time::Duration::from_millis(1000),
    ) {
        Some(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };

    parse_bluetooth_devices(&output)
}

/// Mac OS Roman code points for bytes 0x80..=0xFF; the lower half is ASCII.
/// `pmset` can print accessory names in this legacy encoding (0xD5 is the
/// typographic apostrophe in "Someone’s Keyboard").
#[rustfmt::skip]
const MAC_ROMAN_HIGH: [char; 128] = [
    'Ä', 'Å', 'Ç', 'É', 'Ñ', 'Ö', 'Ü', 'á', // 0x80
    'à', 'â', 'ä', 'ã', 'å', 'ç', 'é', 'è', // 0x88
    'ê', 'ë', 'í', 'ì', 'î', 'ï', 'ñ', 'ó', // 0x90
    'ò', 'ô', 'ö', 'õ', 'ú', 'ù', 'û', 'ü', // 0x98
    '†', '°', '¢', '£', '§', '•', '¶', 'ß', // 0xA0
    '®', '©', '™', '´', '¨', '≠', 'Æ', 'Ø', // 0xA8
    '∞', '±', '≤', '≥', '¥', 'µ', '∂', '∑', // 0xB0
    '∏', 'π', '∫', 'ª', 'º', 'Ω', 'æ', 'ø', // 0xB8
    '¿', '¡', '¬', '√', 'ƒ', '≈', '∆', '«', // 0xC0
    '»', '…', '\u{00A0}', 'À', 'Ã', 'Õ', 'Œ', 'œ', // 0xC8
    '–', '—', '“', '”', '‘', '’', '÷', '◊', // 0xD0
    'ÿ', 'Ÿ', '⁄', '€', '‹', '›', 'ﬁ', 'ﬂ', // 0xD8
    '‡', '·', '‚', '„', '‰', 'Â', 'Ê', 'Á', // 0xE0
    'Ë', 'È', 'Í', 'Î', 'Ï', 'Ì', 'Ó', 'Ô', // 0xE8
    '\u{F8FF}', 'Ò', 'Ú', 'Û', 'Ù', 'ı', 'ˆ', '˜', // 0xF0
    '¯', '˘', '˙', '˚', '¸', '˝', '˛', 'ˇ', // 0xF8
];

fn decode_mac_roman(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b < 0x80 {
                b as char
            } else {
                MAC_ROMAN_HIGH[(b - 0x80) as usize]
            }
        })
        .collect()
}

fn parse_bluetooth_devices(output: &[u8]) -> Vec<BluetoothDevice> {
    let output = match std::str::from_utf8(output) {
        Ok(text) => std::borrow::Cow::Borrowed(text),
        Err(_) => std::borrow::Cow::Owned(decode_mac_roman(output)),
    };

    struct Entry {
        name: String,
        id: u64,
        pct: String,
        status: String,
    }

    let mut raw: Vec<Entry> = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("-InternalBattery")
            || trimmed.starts_with("Now drawing")
            || trimmed.is_empty()
        {
            continue;
        }
        if let Some(name_end) = trimmed.find(" (id=") {
            let name = trimmed[1..name_end].trim_start_matches('-').trim();
            let id: u64 = trimmed[name_end + 5..]
                .split(')')
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            let after_tab = trimmed.split('\t').nth(1).unwrap_or("");
            let pct = after_tab.split('%').next().unwrap_or("").trim();

            let segments: Vec<&str> = after_tab.split(';').collect();
            let is_charging = segments.len() >= 2
                && segments[1].contains("charging")
                && !segments[1].contains("discharging");
            let mut status = String::new();
            if is_charging {
                let time_seg = segments.get(2).unwrap_or(&"").trim();
                let time = time_seg
                    .replace("remaining", "")
                    .replace("present: true", "")
                    .trim()
                    .to_string();
                if !time.is_empty() && time != "0:00" {
                    status = format!("charging, {} left", time);
                } else if pct.parse::<u32>().unwrap_or(0) >= 100 {
                    status = "charged".to_string();
                } else {
                    status = "charging".to_string();
                }
            }

            if !name.is_empty() && !pct.is_empty() {
                raw.push(Entry {
                    name: name.to_string(),
                    id,
                    pct: format!("{}%", pct),
                    status,
                });
            }
        }
    }

    raw.sort_by_key(|r| r.id);

    let mut name_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for e in &raw {
        if !e.name.ends_with(" Case") {
            *name_counts.entry(e.name.clone()).or_insert(0) += 1;
        }
    }

    let mut devices: Vec<BluetoothDevice> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for e in &raw {
        let display_name = if e.name.ends_with(" Case") {
            e.name.clone()
        } else {
            let count = name_counts.get(&e.name).copied().unwrap_or(1);
            if count >= 2 {
                let nth = seen.entry(e.name.clone()).or_insert(0);
                let side = format!("#{}", *nth + 1);
                *nth += 1;
                format!("{} ({})", e.name, side)
            } else {
                e.name.clone()
            }
        };

        let battery_str = if e.status.is_empty() {
            e.pct.clone()
        } else {
            format!("{}, {}", e.pct, e.status)
        };

        devices.push(BluetoothDevice {
            name: display_name,
            minor_type: String::new(),
            connected: true,
            batteries: vec![(String::new(), battery_str)],
        });
    }

    devices
}

#[cfg(test)]
mod bluetooth_tests {
    use super::parse_bluetooth_devices;

    #[test]
    fn parses_mac_roman_bluetooth_device_names_without_replacement_chars() {
        let output = b"Now drawing from 'AC Power'\n\
 -Sample\xD5s Magic Keyboard (id=58458112)\t97%; discharging present: true\n";

        let devices = parse_bluetooth_devices(output);

        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "Sample\u{2019}s Magic Keyboard");
        assert!(!devices[0].name.contains('\u{FFFD}'));
    }

    #[test]
    fn mac_roman_table_matches_the_encoding() {
        use super::decode_mac_roman;
        assert_eq!(decode_mac_roman(b"plain ASCII ~"), "plain ASCII ~");
        // Corners and a sample of each row of the high half.
        assert_eq!(
            decode_mac_roman(&[0x80, 0x8E, 0xA5, 0xB9, 0xCA, 0xD0, 0xD5, 0xDB, 0xF0, 0xFF]),
            "Äé•π\u{A0}–’€\u{F8FF}ˇ"
        );
        // Every byte maps to exactly one character, none of them a replacement.
        let all: Vec<u8> = (0..=255).collect();
        let text = decode_mac_roman(&all);
        assert_eq!(text.chars().count(), 256);
        assert!(!text.contains('\u{FFFD}'));
    }

    #[test]
    fn preserves_utf8_bluetooth_device_names() {
        let output = "Now drawing from 'AC Power'\n\
 -Sample’s Keyboard (id=58458112)\t97%; discharging present: true\n";

        let devices = parse_bluetooth_devices(output.as_bytes());

        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "Sample’s Keyboard");
    }
}
