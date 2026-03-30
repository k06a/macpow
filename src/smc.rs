use crate::iokit_ffi::*;
use crate::types::{FanInfo, TempSensor};
use anyhow::{bail, Result};

// ── SMC protocol structs (must match kernel layout exactly) ─────────────────

const KERNEL_INDEX_SMC: u32 = 2;
const SMC_CMD_READ_KEYINFO: u8 = 9;
const SMC_CMD_READ_BYTES: u8 = 5;
const SMC_CMD_READ_INDEX: u8 = 8;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SmcVers {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SmcPLimitData {
    version: u16,
    length: u16,
    cpu_p_limit: u32,
    gpu_p_limit: u32,
    mem_p_limit: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct SmcKeyInfoData {
    data_size: u32,
    data_type: u32,
    data_attributes: u8,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SmcKeyData {
    key: u32,
    vers: SmcVers,
    p_limit_data: SmcPLimitData,
    key_info: SmcKeyInfoData,
    result: u8,
    status: u8,
    data8: u8,
    data32: u32,
    bytes: [u8; 32],
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn fourcc(s: &str) -> u32 {
    let b = s.as_bytes();
    if b.len() < 4 {
        return 0;
    }
    (b[0] as u32) << 24 | (b[1] as u32) << 16 | (b[2] as u32) << 8 | (b[3] as u32)
}

#[allow(dead_code)]
fn fourcc_to_str(v: u32) -> String {
    let bytes = [(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8];
    String::from_utf8_lossy(&bytes).into_owned()
}

/// FourCC for `flt ` (IEEE 754 float, with trailing space).
const TYPE_FLT: u32 = 0x666c7420; // 'f','l','t',' '
/// FourCC for `iof ` (IO float).
const TYPE_IOF: u32 = 0x696f6620;
/// FourCC for `sp78` (signed fixed-point 7.8).
const TYPE_SP78: u32 = 0x73703738;
/// FourCC for `ui32`.
#[allow(dead_code)]
const TYPE_UI32: u32 = 0x75693332;

fn bytes_to_f32_le(b: &[u8]) -> f32 {
    if b.len() < 4 {
        return 0.0;
    }
    f32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn bytes_to_sp78(b: &[u8]) -> f32 {
    if b.len() < 2 {
        return 0.0;
    }
    let raw = i16::from_be_bytes([b[0], b[1]]);
    raw as f32 / 256.0
}

// ── public interface ─────────────────────────────────────────────────────────

pub struct SmcConnection {
    conn: u32,
    temp_keys: Option<Vec<(String, String)>>,
}

unsafe impl Send for SmcConnection {}

impl SmcConnection {
    pub fn open() -> Result<Self> {
        unsafe {
            let matching = IOServiceMatching(b"AppleSMC\0".as_ptr() as *const i8);
            if matching.is_null() {
                bail!("SMC: IOServiceMatching returned null");
            }
            let service = IOServiceGetMatchingService(0, matching);
            if service == 0 {
                bail!("SMC: no AppleSMC service found");
            }

            let mut conn: u32 = 0;
            let kr = IOServiceOpen(service, mach_task_self(), 0, &mut conn);
            IOObjectRelease(service);
            if kr != 0 {
                bail!("SMC: IOServiceOpen failed (0x{:x})", kr);
            }
            Ok(Self {
                conn,
                temp_keys: None,
            })
        }
    }

    /// Start async temperature key discovery. Call `finish_temp_discovery`
    /// later to collect results without blocking.
    pub fn start_temp_discovery(&self) -> std::thread::JoinHandle<Vec<(String, String)>> {
        // Open a second SMC connection for the background thread
        let conn2 = Self::open_raw_conn().unwrap_or(0);
        std::thread::spawn(move || {
            if conn2 == 0 {
                return Vec::new();
            }
            let tmp = SmcConnection {
                conn: conn2,
                temp_keys: None,
            };
            let keys = tmp.discover_temp_keys();
            // Don't close via Drop — we manually close
            unsafe {
                IOServiceClose(conn2);
            }
            std::mem::forget(tmp);
            keys
        })
    }

    fn open_raw_conn() -> Option<u32> {
        unsafe {
            let matching = IOServiceMatching(b"AppleSMC\0".as_ptr() as *const i8);
            if matching.is_null() {
                return None;
            }
            let service = IOServiceGetMatchingService(0, matching);
            if service == 0 {
                return None;
            }
            let mut conn: u32 = 0;
            let kr = IOServiceOpen(service, mach_task_self(), 0, &mut conn);
            IOObjectRelease(service);
            if kr != 0 {
                None
            } else {
                Some(conn)
            }
        }
    }

    pub fn finish_temp_discovery(
        &mut self,
        handle: std::thread::JoinHandle<Vec<(String, String)>>,
    ) {
        if let Ok(keys) = handle.join() {
            self.temp_keys = Some(keys);
        }
    }

    fn call(&self, input: &SmcKeyData) -> Result<SmcKeyData> {
        unsafe {
            let mut output = SmcKeyData::default();
            let mut out_size = std::mem::size_of::<SmcKeyData>();
            let kr = IOConnectCallStructMethod(
                self.conn,
                KERNEL_INDEX_SMC,
                input as *const SmcKeyData as *const u8,
                std::mem::size_of::<SmcKeyData>(),
                &mut output as *mut SmcKeyData as *mut u8,
                &mut out_size,
            );
            if kr != 0 {
                bail!("SMC call failed (0x{:x})", kr);
            }
            Ok(output)
        }
    }

    fn read_key_info(&self, key: u32) -> Result<SmcKeyInfoData> {
        let mut input = SmcKeyData::default();
        input.key = key;
        input.data8 = SMC_CMD_READ_KEYINFO;
        let out = self.call(&input)?;
        Ok(out.key_info)
    }

    pub fn read_key_raw(&self, key_str: &str) -> Result<(SmcKeyInfoData, [u8; 32])> {
        let key = fourcc(key_str);
        let info = self.read_key_info(key)?;

        let mut input = SmcKeyData::default();
        input.key = key;
        input.data8 = SMC_CMD_READ_BYTES;
        input.key_info = info;

        let out = self.call(&input)?;
        Ok((info, out.bytes))
    }

    /// Read a float value from an SMC key (handles `flt `, `iof `, `sp78` types).
    pub fn read_f32(&self, key_str: &str) -> Result<f32> {
        let (info, bytes) = self.read_key_raw(key_str)?;
        let val = match info.data_type {
            TYPE_FLT | TYPE_IOF => bytes_to_f32_le(&bytes),
            TYPE_SP78 => bytes_to_sp78(&bytes),
            _ => bytes_to_f32_le(&bytes),
        };
        Ok(val)
    }

    // ── SMC key enumeration ────────────────────────────────────────────────

    /// Get total number of keys in the SMC.
    fn key_count(&self) -> u32 {
        let key = fourcc("#KEY");
        let mut input = SmcKeyData::default();
        input.key = key;
        input.data8 = SMC_CMD_READ_KEYINFO;
        if let Ok(out) = self.call(&input) {
            let info = out.key_info;
            // Now read the value
            let mut input2 = SmcKeyData::default();
            input2.key = key;
            input2.data8 = SMC_CMD_READ_BYTES;
            input2.key_info = info;
            if let Ok(out2) = self.call(&input2) {
                return u32::from_be_bytes([
                    out2.bytes[0],
                    out2.bytes[1],
                    out2.bytes[2],
                    out2.bytes[3],
                ]);
            }
        }
        0
    }

    /// Get the FourCC key at a given index.
    fn key_at_index(&self, index: u32) -> Option<u32> {
        let mut input = SmcKeyData::default();
        input.data8 = SMC_CMD_READ_INDEX;
        input.data32 = index;
        if let Ok(out) = self.call(&input) {
            if out.key != 0 {
                return Some(out.key);
            }
        }
        None
    }

    // ── temperatures ─────────────────────────────────────────────────────────

    pub fn read_temperatures(&mut self) -> Vec<TempSensor> {
        let keys = match self.temp_keys.as_ref() {
            Some(k) => k,
            None => return Vec::new(),
        };
        keys.iter()
            .filter_map(|(key_str, category)| {
                let val = self.read_f32(key_str).ok()?;
                (val > 0.0 && val < 150.0).then(|| TempSensor {
                    key: key_str.clone(),
                    category: category.clone(),
                    value_celsius: val,
                })
            })
            .collect()
    }

    fn discover_temp_keys(&self) -> Vec<(String, String)> {
        let prefixes: &[(&str, &str)] = &[
            ("Tp", "CPU"),
            ("Te", "CPU"),
            ("Tg", "GPU"),
            ("Ts", "SSD"),
            ("Tm", "Memory"),
            ("TB", "Battery"),
            ("Tw", "Wireless"),
            ("Ta", "ANE"),
        ];

        let count = self.key_count();
        (0..count)
            .filter_map(|i| {
                let key_fcc = self.key_at_index(i)?;
                ((key_fcc >> 24) as u8 == b'T').then_some(())?;
                let key_str = fourcc_to_str(key_fcc);
                let info = self.read_key_info(key_fcc).ok()?;
                matches!(info.data_type, TYPE_FLT | TYPE_IOF | TYPE_SP78).then_some(())?;
                let category = prefixes
                    .iter()
                    .find(|(p, _)| key_str.starts_with(p))
                    .map(|(_, c)| c.to_string())
                    .unwrap_or_else(|| "Other".into());
                Some((key_str, category))
            })
            .collect()
    }

    // ── fans ─────────────────────────────────────────────────────────────────

    pub fn read_fans(&self) -> Vec<FanInfo> {
        let num_fans = self
            .read_key_raw("FNum")
            .ok()
            .filter(|(info, _)| info.data_size >= 1)
            .map(|(_, bytes)| bytes[0] as u32)
            .unwrap_or(0);

        (0..num_fans.min(8))
            .filter_map(|i| {
                let actual = self.read_f32(&format!("F{}Ac", i)).ok()?;
                let min = self.read_f32(&format!("F{}Mn", i)).unwrap_or(0.0);
                let max = self.read_f32(&format!("F{}Mx", i)).unwrap_or(0.0);
                let power = if max > 0.0 {
                    let ratio = (actual / max).clamp(0.0, 1.0);
                    crate::metrics::MAX_FAN_W * ratio.powi(3)
                } else {
                    0.0
                };
                Some(FanInfo {
                    id: i,
                    name: format!("Fan {}", i),
                    actual_rpm: actual,
                    min_rpm: min,
                    max_rpm: max,
                    estimated_power_w: power,
                })
            })
            .collect()
    }

    // ── keyboard backlight ───────────────────────────────────────────────────

    /// Read keyboard backlight level (0.0–1.0). Returns 0 if not available.
    #[allow(dead_code)]
    pub fn read_keyboard_backlight(&self) -> f32 {
        // Try LKSB key first, then LKBR
        if let Ok((info, bytes)) = self.read_key_raw("LKSB") {
            if info.data_size >= 2 {
                return bytes[1] as f32 / 255.0;
            }
            if info.data_size == 1 {
                return bytes[0] as f32 / 255.0;
            }
        }
        if let Ok(v) = self.read_f32("LKBR") {
            return v;
        }
        0.0
    }

    // ── system power ─────────────────────────────────────────────────────────

    /// Read total system power from SMC `PSTR` key (watts). Returns 0 if unavailable.
    pub fn read_system_power(&self) -> f32 {
        self.read_f32("PSTR").unwrap_or(0.0)
    }
}

impl Drop for SmcConnection {
    fn drop(&mut self) {
        unsafe {
            IOServiceClose(self.conn);
        }
    }
}
