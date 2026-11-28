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
    pub data_size: u32,
    pub data_type: u32,
    pub data_attributes: u8,
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
/// FourCC for `ioft` (unsigned fixed-point 48.16, little-endian).
const TYPE_IOFT: u32 = u32::from_be_bytes(*b"ioft");
/// FourCC for `sp78` (signed fixed-point 7.8).
const TYPE_SP78: u32 = 0x73703738;
/// FourCC for `ui32`.
#[allow(dead_code)]
const TYPE_UI32: u32 = 0x75693332;

/// Decode the payload of a numeric SMC key (`flt `, `ioft`, `sp78`).
///
/// Returns `None` for any other type and for a payload shorter than its type
/// needs: reading such bytes as a float would produce a number that looks
/// like a measurement but is not one.
fn decode_f32(data_type: u32, payload: &[u8]) -> Option<f32> {
    match data_type {
        TYPE_FLT => Some(f32::from_le_bytes(payload.get(..4)?.try_into().ok()?)),
        TYPE_IOFT => {
            let raw = u64::from_le_bytes(payload.get(..8)?.try_into().ok()?);
            Some(raw as f32 / 65536.0)
        }
        TYPE_SP78 => {
            let raw = i16::from_be_bytes(payload.get(..2)?.try_into().ok()?);
            Some(raw as f32 / 256.0)
        }
        _ => None,
    }
}

// ── Raw key dump ─────────────────────────────────────────────────────────────

/// One enumerated SMC key plus its raw type info and payload bytes.
/// Output of [`SmcConnection::dump_all`].
#[derive(Clone)]
pub struct RawSmcEntry {
    /// Four-character key name, e.g. "PSTR", "PBwo", "TC0c".
    pub key: String,
    /// Four-character type tag, e.g. "flt", "sp78", "ui32" (trailing space stripped).
    pub data_type: String,
    /// Number of valid payload bytes (≤ 32).
    pub data_size: u32,
    /// Raw 32-byte SMC payload buffer (only the first `data_size` bytes are valid).
    pub bytes: [u8; 32],
}

impl RawSmcEntry {
    /// Format the payload as space-separated lowercase hex pairs.
    pub fn bytes_hex(&self) -> String {
        let n = (self.data_size as usize).min(32);
        let mut s = String::with_capacity(n * 3);
        for (i, b) in self.bytes[..n].iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            s.push_str(&format!("{:02x}", b));
        }
        s
    }

    /// Decode the payload to a human-readable string for the well-known SMC types.
    /// Mirrors iSMC's decoding for cross-reference with their `reports/` dumps,
    /// except for the byte order of integers (see [`decode_smc_value`]).
    /// Returns an empty string for types we don't understand — the caller can
    /// still show the raw hex.
    pub fn decoded(&self) -> String {
        let mut bytes = self.bytes;
        if is_big_endian_key(&self.key) {
            // Reversed, a big-endian integer is the little-endian one that
            // decode_smc_value reads.
            let n = (self.data_size as usize).min(32);
            bytes[..n].reverse();
        }
        decode_smc_value(&self.data_type, &bytes, self.data_size)
    }
}

/// Whether an integer key is big-endian, unlike most Apple Silicon SMC
/// integers. The byte order is a property of the key, not of its type, so
/// this lists the keys that were checked on a Mac17,7 (M5 Max) against a
/// source outside the SMC:
/// * `#KEY` – the count the key enumeration walks.
/// * `RBID`, `RCID`, `RCRV` – `board-id`, `chip-id` and `chip-revision` of
///   the device tree. `RBRV` (board revision) has nothing to compare with and
///   is listed as the fourth key of that boot-identity group.
/// * `D<n>CF`, `D<n>CR`, `D<n>Rc`, `D<n>FV`, `D<n>Ac` – `SrdyCount`,
///   `SrdoCount`, `HardResetCount`, `FwVersion` and `ActiveContractRdo` of
///   USB-C port controller n in the registry (`PortController…` properties).
///   Their neighbours `D<n>CA`/`D<n>CD` (attach/detach count) are
///   little-endian.
fn is_big_endian_key(key: &str) -> bool {
    match key.as_bytes() {
        b"#KEY" | b"RBID" | b"RBRV" | b"RCID" | b"RCRV" => true,
        [b'D', b'1'..=b'9', field @ ..] => {
            matches!(field, b"CF" | b"CR" | b"Rc" | b"FV" | b"Ac")
        }
        _ => false,
    }
}

/// Decode raw SMC payload bytes for known type tags.
///
/// Supported families (from iSMC's `smc/conv.go`, GPL-3.0):
/// * `flt`      – IEEE 754 float, little-endian
/// * `ioft`     – 48.16 unsigned fixed-point, little-endian
/// * `fp1f`–`fpe2` – unsigned fixed-point Q*.* (big-endian)
/// * `sp1e`–`spf0` – signed fixed-point Q*.* (big-endian)
/// * `ui8`/`ui16`/`ui32`/`hex_` – little-endian unsigned integer (payloads of
///   up to 4 bytes; a longer `hex_` blob has no single value)
/// * `si8`/`si16`/`si32` – little-endian signed integer
/// * `flag`     – 0/1 boolean (rendered as "true"/"false")
/// * `pwm`      – 16-bit big-endian percentage of 0xFFFF
///
/// iSMC reads integers big-endian, the Intel SMC convention. On Apple Silicon
/// most are little-endian: `B0CT` (battery cycle count, `ui16`) with bytes
/// `f1 00` is 241, as the battery registry entry confirms. The keys known to
/// be big-endian anyway are handled by [`RawSmcEntry::decoded`]; for any other
/// key the raw bytes printed next to the value remain the reference.
pub fn decode_smc_value(data_type: &str, bytes: &[u8; 32], size: u32) -> String {
    if size == 0 {
        return String::new();
    }
    let n = (size as usize).min(32);
    let buf = &bytes[..n];
    let t = data_type.trim_end_matches(['\0', ' ']);

    match t {
        "flt" => {
            if n < 4 {
                return String::new();
            }
            let v = f32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            format!("{}", v)
        }
        "ioft" => {
            if n < 8 {
                return String::new();
            }
            let raw = u64::from_le_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]);
            format!("{}", raw as f32 / 65536.0)
        }
        "ui8" | "ui16" | "ui32" | "hex_" => match le_uint(buf) {
            Some(v) => format!("{}", v),
            None => String::new(),
        },
        "si8" => format!("{}", buf[0] as i8),
        "si16" => {
            if n < 2 {
                return String::new();
            }
            format!("{}", i16::from_le_bytes([buf[0], buf[1]]))
        }
        "si32" => {
            if n < 4 {
                return String::new();
            }
            format!("{}", i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
        }
        "flag" => format!("{}", le_uint(buf) == Some(1)),
        "pwm" => {
            if n < 2 {
                return String::new();
            }
            let raw = u16::from_be_bytes([buf[0], buf[1]]);
            format!("{:.1}%", raw as f32 * 100.0 / 65536.0)
        }
        other => match fp_divisor(other) {
            Some((div, signed)) => {
                if n < 2 {
                    return String::new();
                }
                let raw = u16::from_be_bytes([buf[0], buf[1]]);
                let v = if signed {
                    raw as i16 as f32 / div
                } else {
                    raw as f32 / div
                };
                format!("{}", v)
            }
            None => String::new(),
        },
    }
}

/// Little-endian unsigned integer made of the whole payload.
///
/// `None` when the payload is empty or longer than 4 bytes: a `hex_` blob can
/// be up to 32 bytes of packed fields, and a number built from its first four
/// would be an invented value.
fn le_uint(buf: &[u8]) -> Option<u32> {
    if buf.is_empty() || buf.len() > 4 {
        return None;
    }
    let mut le = [0u8; 4];
    le[..buf.len()].copy_from_slice(buf);
    Some(u32::from_le_bytes(le))
}

/// Returns `(divisor, signed)` for fixed-point fp/sp type tags.
/// Naming convention: the digit pair encodes the integer/fraction split, e.g.
/// `sp78` is signed Q7.8 → divisor 256 (= 1<<8), `fpe2` is unsigned Q14.2 → divisor 4 (= 1<<2).
fn fp_divisor(t: &str) -> Option<(f32, bool)> {
    if t.len() != 4 {
        return None;
    }
    let bytes = t.as_bytes();
    let signed = match bytes[0] {
        b'f' if bytes[1] == b'p' => false,
        b's' if bytes[1] == b'p' => true,
        _ => return None,
    };
    let frac = match bytes[3] {
        c @ b'0'..=b'9' => (c - b'0') as u32,
        c @ b'a'..=b'f' => (c - b'a' + 10) as u32,
        _ => return None,
    };
    let div = (1u32 << frac) as f32;
    Some((div, signed))
}

// ── public interface ─────────────────────────────────────────────────────────

pub struct SmcConnection {
    conn: u32,
    temp_keys: Option<Vec<(String, String)>>,
    key_info_cache: std::collections::HashMap<u32, SmcKeyInfoData>,
    temp_value_cache: std::collections::HashMap<String, (String, f32)>,
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
                key_info_cache: std::collections::HashMap::new(),
                temp_value_cache: std::collections::HashMap::new(),
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
            let mut tmp = SmcConnection {
                conn: conn2,
                temp_keys: None,
                key_info_cache: std::collections::HashMap::new(),
                temp_value_cache: std::collections::HashMap::new(),
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

    fn read_key_info(&mut self, key: u32) -> Result<SmcKeyInfoData> {
        if let Some(&cached) = self.key_info_cache.get(&key) {
            return Ok(cached);
        }
        let mut input = SmcKeyData::default();
        input.key = key;
        input.data8 = SMC_CMD_READ_KEYINFO;
        let out = self.call(&input)?;
        self.key_info_cache.insert(key, out.key_info);
        Ok(out.key_info)
    }

    pub fn read_key_raw(&mut self, key_str: &str) -> Result<(SmcKeyInfoData, [u8; 32])> {
        let key = fourcc(key_str);
        let info = self.read_key_info(key)?;
        // Some Apple SMCs return kr=0 with size=0/type=0 when the FOURCC isn't
        // present (e.g. `PBwo` on M5 Max). Treat that as a missing key so callers
        // can `or_else` to a fallback key correctly.
        if info.data_size == 0 || info.data_type == 0 {
            bail!("SMC key {:?} not present (data_size=0)", key_str);
        }

        let mut input = SmcKeyData::default();
        input.key = key;
        input.data8 = SMC_CMD_READ_BYTES;
        input.key_info = info;

        let out = self.call(&input)?;
        Ok((info, out.bytes))
    }

    /// Read a float value from an SMC key (handles `flt `, `ioft`, `sp78` types).
    /// Fails for a key of any other type.
    pub fn read_f32(&mut self, key_str: &str) -> Result<f32> {
        let (info, bytes) = self.read_key_raw(key_str)?;
        let size = (info.data_size as usize).min(bytes.len());
        match decode_f32(info.data_type, &bytes[..size]) {
            Some(val) => Ok(val),
            None => bail!(
                "SMC key {:?}: cannot read type {:?} ({} bytes) as a float",
                key_str,
                fourcc_to_str(info.data_type),
                info.data_size
            ),
        }
    }

    // ── SMC key enumeration ────────────────────────────────────────────────

    /// Get total number of keys in the SMC.
    pub fn key_count(&self) -> u32 {
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
    pub fn key_at_index(&self, index: u32) -> Option<u32> {
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

    /// Enumerate every SMC key and return a snapshot of its type and bytes.
    /// Output order matches the SMC's internal index ordering (alphabetical).
    /// Skips keys whose info or value read fails.
    pub fn dump_all(&mut self) -> Vec<RawSmcEntry> {
        let count = self.key_count();
        // Sane upper bound — current Apple Silicon Macs report ~1500–2700 keys.
        // Guards against a corrupt #KEY value.
        let count = count.min(8192);
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let Some(key_fcc) = self.key_at_index(i) else {
                continue;
            };
            let Ok((info, bytes)) = self.read_key_raw_by_fourcc(key_fcc) else {
                continue;
            };
            out.push(RawSmcEntry {
                key: fourcc_to_str(key_fcc),
                data_type: fourcc_to_str(info.data_type),
                data_size: info.data_size,
                bytes,
            });
        }
        out
    }

    fn read_key_raw_by_fourcc(&mut self, key: u32) -> Result<(SmcKeyInfoData, [u8; 32])> {
        let info = self.read_key_info(key)?;
        let mut input = SmcKeyData::default();
        input.key = key;
        input.data8 = SMC_CMD_READ_BYTES;
        input.key_info = info;
        let out = self.call(&input)?;
        Ok((info, out.bytes))
    }

    // ── temperatures ─────────────────────────────────────────────────────────

    pub fn read_temperatures(&mut self) -> Vec<TempSensor> {
        let keys = match self.temp_keys.clone() {
            Some(k) => k,
            None => return Vec::new(),
        };
        let mut result = Vec::with_capacity(keys.len());
        for (key_str, category) in &keys {
            if let Ok(val) = self.read_f32(key_str) {
                if val > 0.0 && val < 150.0 {
                    self.temp_value_cache
                        .insert(key_str.clone(), (category.clone(), val));
                    result.push(TempSensor {
                        key: key_str.clone(),
                        category: category.clone(),
                        value_celsius: val,
                        stale: false,
                    });
                    continue;
                }
            }
            // Read failed or value out of range — use cached value if available
            if let Some((cached_cat, cached_val)) = self.temp_value_cache.get(key_str) {
                result.push(TempSensor {
                    key: key_str.clone(),
                    category: cached_cat.clone(),
                    value_celsius: *cached_val,
                    stale: true,
                });
            }
        }
        result
    }

    fn discover_temp_keys(&mut self) -> Vec<(String, String)> {
        // Longer prefixes first: TPMP/TPSP must match before Tp (CPU)
        let prefixes: &[(&str, &str)] = &[
            ("TPMP", "Trackpad"),
            ("TPSP", "Trackpad"),
            ("TD", "Display"),
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
                matches!(info.data_type, TYPE_FLT | TYPE_IOFT | TYPE_SP78).then_some(())?;
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

    pub fn read_fans(&mut self) -> Vec<FanInfo> {
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
    pub fn read_keyboard_backlight(&mut self) -> f32 {
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
    pub fn read_system_power(&mut self) -> f32 {
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

#[cfg(test)]
mod tests {
    use super::*;

    // Payload bytes in these tests were captured with `macpow --dump-smc` on a
    // MacBook Pro Mac17,7 (M5 Max).

    #[test]
    fn type_tags_spell_their_names() {
        assert_eq!(TYPE_FLT, fourcc("flt "));
        assert_eq!(TYPE_IOFT, fourcc("ioft"));
        assert_eq!(TYPE_SP78, fourcc("sp78"));
        assert_eq!(TYPE_UI32, fourcc("ui32"));
        assert_eq!(fourcc_to_str(TYPE_IOFT), "ioft");
    }

    #[test]
    fn decode_f32_reads_flt_little_endian() {
        // F0Ac: fan at 2330 rpm.
        assert_eq!(
            decode_f32(TYPE_FLT, &[0x00, 0xa0, 0x11, 0x45]),
            Some(2330.0)
        );
    }

    #[test]
    fn decode_f32_reads_ioft_as_48_16_fixed_point() {
        // TG0C and TG2B.
        assert_eq!(
            decode_f32(TYPE_IOFT, &[0x00, 0x00, 0x22, 0, 0, 0, 0, 0]),
            Some(34.0)
        );
        assert_eq!(
            decode_f32(TYPE_IOFT, &[0x00, 0x80, 0x22, 0, 0, 0, 0, 0]),
            Some(34.5)
        );
        // TG0B (ioft) and TB0T (flt) are the same battery temperature, read
        // in one dump: both payloads must decode to the same 36.8 °C.
        assert_eq!(
            decode_f32(TYPE_IOFT, &[0xcc, 0xcc, 0x24, 0, 0, 0, 0, 0]),
            decode_f32(TYPE_FLT, &[0x30, 0x33, 0x13, 0x42])
        );
        // The integer part is not limited to the low 32 bits.
        assert_eq!(
            decode_f32(TYPE_IOFT, &[0, 0, 0, 0, 0x01, 0, 0, 0]),
            Some(65536.0)
        );
    }

    #[test]
    fn decode_f32_reads_sp78_big_endian() {
        assert_eq!(decode_f32(TYPE_SP78, &[0x1e, 0x80]), Some(30.5));
        assert_eq!(decode_f32(TYPE_SP78, &[0xff, 0x80]), Some(-0.5));
    }

    #[test]
    fn decode_f32_rejects_types_that_are_not_floats() {
        // B0CT (battery cycle count) and #KEY (number of keys) are integers.
        assert_eq!(decode_f32(fourcc("ui16"), &[0xf1, 0x00]), None);
        assert_eq!(decode_f32(TYPE_UI32, &[0x00, 0x00, 0x0f, 0x18]), None);
        // "iof " with a space is not an SMC type, only "ioft" is.
        assert_eq!(
            decode_f32(fourcc("iof "), &[0x33, 0x33, 0x23, 0, 0, 0, 0, 0]),
            None
        );
    }

    #[test]
    fn decode_f32_rejects_payloads_shorter_than_the_type() {
        assert_eq!(decode_f32(TYPE_FLT, &[0x00, 0xa0, 0x11]), None);
        assert_eq!(decode_f32(TYPE_IOFT, &[0x33, 0x33, 0x23, 0x00]), None);
        assert_eq!(decode_f32(TYPE_SP78, &[0x1e]), None);
        assert_eq!(decode_f32(TYPE_FLT, &[]), None);
    }

    /// The decoded column of `--dump-smc` for one key, built the way
    /// `dump_all` builds its entries.
    fn dumped(key: &str, data_type: &str, payload: &[u8]) -> String {
        let mut bytes = [0u8; 32];
        bytes[..payload.len()].copy_from_slice(payload);
        RawSmcEntry {
            key: key.into(),
            data_type: data_type.into(),
            data_size: payload.len() as u32,
            bytes,
        }
        .decoded()
    }

    #[test]
    fn dump_decodes_integers_little_endian() {
        // The battery registry entry showed CycleCount 241 and DesignCapacity
        // 6249 next to these two payloads.
        assert_eq!(dumped("B0CT", "ui16", &[0xf1, 0x00]), "241");
        assert_eq!(dumped("B0DC", "ui16", &[0x69, 0x18]), "6249");
        // Battery voltage in mV and current in mA (negative: discharging).
        assert_eq!(dumped("B0AV", "ui16", &[0x54, 0x30]), "12372");
        assert_eq!(dumped("B0AC", "si16", &[0x51, 0xf9]), "-1711");
        assert_eq!(dumped("CHBV", "ui32", &[0x20, 0x11, 0x00, 0x00]), "4384");
        assert_eq!(dumped("TVDi", "si32", &[0x02, 0x00, 0x00, 0x00]), "2");
        assert_eq!(dumped("B0AP", "si32", &[0xd4, 0x83, 0xff, 0xff]), "-31788");
    }

    #[test]
    fn dump_decodes_single_byte_types() {
        assert_eq!(dumped("BRSC", "ui8 ", &[0x46]), "70");
        assert_eq!(dumped("AC-B", "si8 ", &[0xff]), "-1");
        assert_eq!(dumped("AC-S", "flag", &[0x01]), "true");
        assert_eq!(dumped("AC-E", "flag", &[0x00]), "false");
    }

    #[test]
    fn dump_decodes_short_hex_and_leaves_longer_blobs_as_raw_bytes() {
        // The battery registry entry showed ChemID 29795 and GaugeFlagRaw 192.
        assert_eq!(dumped("B0CI", "hex_", &[0x63, 0x74, 0x00, 0x00]), "29795");
        assert_eq!(dumped("B0FI", "hex_", &[0xc0, 0x00]), "192");
        // BTTA packs four 16-bit fields; it has no single value.
        assert_eq!(
            dumped("BTTA", "hex_", &[0x0c, 0, 0x0f, 0, 0x14, 0, 0x2d, 0]),
            ""
        );
    }

    #[test]
    fn dump_keeps_big_endian_keys_big_endian() {
        // 3864 keys; chip-id 0x6050, board-id 10 and chip-revision 0x11 in
        // the device tree.
        assert_eq!(dumped("#KEY", "ui32", &[0x00, 0x00, 0x0f, 0x18]), "3864");
        assert_eq!(dumped("RCID", "ui32", &[0x00, 0x00, 0x60, 0x50]), "24656");
        assert_eq!(dumped("RBID", "ui32", &[0x00, 0x00, 0x00, 0x0a]), "10");
        assert_eq!(dumped("RCRV", "ui32", &[0x00, 0x00, 0x00, 0x11]), "17");
        // The same bytes under any other key are little-endian.
        assert_eq!(
            dumped("CHBV", "ui32", &[0x00, 0x00, 0x0f, 0x18]),
            "403636224"
        );
    }

    #[test]
    fn dump_follows_the_byte_order_of_each_port_controller_key() {
        // Values of the PortController… registry properties of USB-C ports 1
        // and 3 at the time of the dump: SrdyCount 363 and 2344, SrdoCount
        // 285, HardResetCount 10, FwVersion 3211776, ActiveContractRdo
        // 327360712.
        assert_eq!(dumped("D1CF", "ui16", &[0x01, 0x6b]), "363");
        assert_eq!(dumped("D3CF", "ui16", &[0x09, 0x28]), "2344");
        assert_eq!(dumped("D1CR", "ui16", &[0x01, 0x1d]), "285");
        assert_eq!(dumped("D3Rc", "ui16", &[0x00, 0x0a]), "10");
        assert_eq!(dumped("D1FV", "hex_", &[0x00, 0x31, 0x02, 0x00]), "3211776");
        assert_eq!(
            dumped("D3Ac", "hex_", &[0x13, 0x83, 0x20, 0xc8]),
            "327360712"
        );
        // AttachCount 96 and 1034, DetachCount 97: little-endian neighbours.
        assert_eq!(dumped("D1CA", "ui16", &[0x60, 0x00]), "96");
        assert_eq!(dumped("D3CA", "ui16", &[0x0a, 0x04]), "1034");
        assert_eq!(dumped("D1CD", "ui16", &[0x61, 0x00]), "97");
    }

    #[test]
    fn big_endian_keys_are_matched_exactly() {
        assert!(is_big_endian_key("#KEY"));
        assert!(is_big_endian_key("RBRV"));
        assert!(is_big_endian_key("D9Ac"));
        // Not a port number, not a listed field, not four characters.
        assert!(!is_big_endian_key("DRCF"));
        assert!(!is_big_endian_key("D0CF"));
        assert!(!is_big_endian_key("D1CA"));
        assert!(!is_big_endian_key("D1C"));
        assert!(!is_big_endian_key("B0CT"));
        assert!(!is_big_endian_key(""));
    }

    #[test]
    fn dump_decodes_nothing_from_a_payload_shorter_than_its_type() {
        assert_eq!(dumped("B0AC", "si16", &[0x51]), "");
        assert_eq!(dumped("TVDi", "si32", &[0x02, 0x00]), "");
        assert_eq!(dumped("B0CT", "ui16", &[]), "");
    }

    #[test]
    fn dump_float_and_fixed_point_types_keep_their_byte_order() {
        assert_eq!(dumped("F0Ac", "flt ", &[0x00, 0xa0, 0x11, 0x45]), "2330");
        assert_eq!(dumped("TG0C", "ioft", &[0, 0, 0x22, 0, 0, 0, 0, 0]), "34");
        // Intel-era fixed point is big-endian.
        assert_eq!(dumped("TC0P", "sp78", &[0x1e, 0x80]), "30.5");
        assert_eq!(dumped("F0Ac", "fpe2", &[0x1c, 0x20]), "1800");
    }

    #[test]
    fn le_uint_reads_whole_payloads_of_up_to_four_bytes() {
        assert_eq!(le_uint(&[0x46]), Some(70));
        assert_eq!(le_uint(&[0xf1, 0x00]), Some(241));
        assert_eq!(le_uint(&[0x01, 0x02, 0x03]), Some(0x03_02_01));
        assert_eq!(le_uint(&[0xff; 4]), Some(u32::MAX));
        assert_eq!(le_uint(&[]), None);
        assert_eq!(le_uint(&[0; 5]), None);
    }
}
