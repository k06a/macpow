use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Metrics {
    pub soc: SocPower,
    pub battery: BatteryInfo,
    pub adapter: AdapterInfo,
    pub display: DisplayInfo,
    pub keyboard: KeyboardInfo,
    pub audio: AudioInfo,
    pub network: NetworkInfo,
    pub disk: DiskInfo,
    pub ssd_power_w: f32,
    pub usb_devices: Vec<UsbDevice>,
    pub ethernet: EthernetInfo,
    pub eth_network: NetworkInfo,
    pub wifi: WifiInfo,
    pub wifi_network: NetworkInfo,
    pub bluetooth_devices: Vec<BluetoothDevice>,
    pub bluetooth_power_w: f32,
    pub power_assertions: Vec<PowerAssertion>,
    pub top_processes: Vec<ProcessPower>,
    pub all_procs_power_w: f32,
    pub all_procs_energy_mj: f64,
    pub fans: Vec<FanInfo>,
    pub temperatures: Vec<TempSensor>,
    pub sys_power_w: f32,
    pub backlight_power_w: f32,
    pub adapter_power_w: f32,
    pub wifi_power_w: f32,
    pub usb_power_smc_w: f32,
    pub usb_power_out_w: f32,
    pub usb_power_per_port: Vec<UsbPortPower>,
    pub gpu_cores: u32,
    pub dram_gb: u32,
    pub mem_used_gb: f32,
    pub cpu_usage_pct: Vec<f32>,
    pub ssd_model: String,
    /// True when the IOReport Energy Model counters for CPU, DRAM and fabric
    /// have produced exactly zero power for several seconds in a row. On a
    /// running Mac that is not physically possible, so it means the counters
    /// are not being refreshed (observed on macOS 27 unless a privileged
    /// sampler such as `powermetrics` is running).
    pub energy_counters_stalled: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CpuCluster {
    pub name: String,
    pub total_w: f32,
    pub cores: Vec<CpuCore>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CpuCore {
    pub name: String,
    pub watts: f32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SocPower {
    pub cpu_w: f32,
    pub ecpu_clusters: Vec<CpuCluster>,
    pub pcpu_cluster: CpuCluster,
    pub gpu_w: f32,
    pub gpu_util_device: u32,
    pub gpu_util_renderer: u32,
    pub gpu_util_tiler: u32,
    pub ane_w: f32,
    pub ane_parts: Vec<(String, f32)>,
    pub dram_w: f32,
    pub gpu_sram_w: f32,
    pub isp_w: f32,
    pub display_soc_w: f32,
    pub display_ext_w: f32,
    pub pcie_w: f32,
    pub media_w: f32,
    pub fabric_w: f32,
    pub total_w: f32,
    pub ecpu_freq_mhz: u32,
    pub pcpu_freq_mhz: u32,
    pub gpu_freq_mhz: u32,
}

impl SocPower {
    pub fn compute_total(&mut self) {
        self.total_w = self.cpu_w
            + self.gpu_w
            + self.ane_w
            + self.dram_w
            + self.gpu_sram_w
            + self.isp_w
            + self.display_soc_w
            + self.display_ext_w
            + self.pcie_w
            + self.media_w
            + self.fabric_w;
    }

    pub fn ecpu_total_w(&self) -> f32 {
        self.ecpu_clusters.iter().map(|c| c.total_w).sum()
    }

    pub fn pcpu_total_w(&self) -> f32 {
        self.pcpu_cluster.total_w
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct BatteryInfo {
    pub present: bool,
    pub charging: bool,
    pub voltage_mv: f64,
    pub amperage_ma: f64,
    pub drain_w: f64,
    pub capacity_wh: f64,
    pub current_capacity: i64,
    pub max_capacity: i64,
    pub percent: f64,
    pub time_remaining_min: i64,
    pub external_connected: bool,
    pub temperature_c: f64,
    pub cycle_count: i64,
    pub design_capacity_mah: f64,
    pub max_capacity_mah: f64,
    pub health_pct: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AdapterInfo {
    pub connected: bool,
    pub watts: u32,
    pub voltage_mv: u32,
    pub current_ma: u32,
    pub is_wireless: bool,
}

/// Static class of a display panel, derived from peak luminance.
/// Set once at startup; doesn't change at runtime.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub enum PanelClass {
    /// No HDR pipeline; ≤500 nits typical (Air, base 13" MBP, most external panels).
    #[default]
    Sdr,
    /// HDR-capable but not Apple flagship XDR; 501–999 nits typical.
    Hdr,
    /// Apple XDR mini-LED panel; ≥1000 nits peak (14"/16" MBP Pro/Max, Pro Display XDR).
    Xdr,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DisplayInfo {
    pub brightness_pct: f32,
    pub nits: f32,
    /// Peak (calibrated) panel nits — corresponds to `BrightnessMilliNits.max`.
    /// Static; reflects panel hardware capability, not current state.
    pub max_nits: f32,
    pub estimated_power_w: f32,
    pub available: bool,
    pub width_px: u32,
    pub height_px: u32,
    pub diagonal_inches: f32,
    /// Static panel classification derived from `max_nits` thresholds.
    pub panel_class: PanelClass,
    /// Current refresh rate in Hz (e.g. 60.0, 120.0, fractional ProMotion values).
    pub refresh_hz: f32,
    /// True if the panel supports ProMotion (variable refresh up to ~120Hz).
    pub supports_promotion: bool,
    /// HDR pipeline currently engaged — content on screen requesting EDR boost.
    /// Derived from AppleARMBacklight `DPB factor` IOReport channel:
    /// `factor > 1.05` (raw value > ~68813 in 16.16 fixed-point).
    pub hdr_active: bool,
    /// Raw `DPB factor` value as 16.16 fixed-point divided to a float (1.0 = no boost).
    pub dpb_factor: f32,
    /// User-selected Reference Mode name (e.g. "Apple Display (P3-600 nits)",
    /// "HDR Video (P3-ST 2084)", "Photography (P3-D65)"). Updates dynamically
    /// when the user changes the preset in System Settings → Displays. Empty
    /// when the API isn't available (some macOS versions or display types).
    pub preset_name: String,
    /// Peak SDR luminance allowed by the active preset (nits).
    pub preset_max_sdr_nits: f32,
    /// Peak HDR luminance allowed by the active preset (nits). Equals
    /// `preset_max_sdr_nits` for SDR-only presets like "P3-600".
    pub preset_max_hdr_nits: f32,
    /// Maximum EDR headroom permitted by the active preset (1.0 means no HDR
    /// boost in this mode; e.g. 5.0 in "P3-600", up to 16.0 in "HDR Video").
    pub preset_max_edr_headroom: f32,
    /// Peak nits the active Reference Mode can reach (the "marketing" cap from
    /// the preset name — 600 in "P3-600", 1600 in "P3-1600 nits", etc.).
    /// Defaults to `preset_max_hdr_nits` if available, otherwise SDR cap.
    /// This is what gets displayed as the "max" in `current/peak nits`.
    pub peak_nits: f32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct KeyboardInfo {
    pub brightness_pct: f32,
    pub estimated_power_w: f32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AudioInfo {
    pub volume_pct: Option<f32>,
    pub muted: bool,
    pub device_active: bool,
    pub playing: bool,
    pub estimated_power_w: f32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NetworkInfo {
    pub bytes_in_per_sec: f64,
    pub bytes_out_per_sec: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct EthernetInfo {
    pub connected: bool,
    pub interface_name: String,
    pub link_speed_mbps: u32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DiskInfo {
    pub read_bytes_per_sec: f64,
    pub write_bytes_per_sec: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UsbPortPower {
    pub port_index: u32,
    pub power_w: f32,
    pub location_id: u32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UsbDevice {
    pub name: String,
    pub vendor_id: u32,
    pub product_id: u32,
    pub power_ma: Option<u32>,
    pub speed: u32,
    pub location_id: u32,
    pub parent_location_id: u32,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WifiInfo {
    pub connected: bool,
    pub interface_name: String,
    pub ssid: String,
    pub rssi_dbm: i32,
    pub noise_dbm: i32,
    pub tx_rate_mbps: f32,
    pub phy_mode: String,
    pub channel: String,
    pub estimated_power_w: f32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct BluetoothDevice {
    pub name: String,
    pub minor_type: String,
    pub connected: bool,
    pub batteries: Vec<(String, String)>, // e.g. [("Left", "93%"), ("Right", "100%"), ("Case", "7%")]
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PowerAssertion {
    pub name: String,
    pub assertion_type: String,
    pub pid: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FanInfo {
    pub id: u32,
    pub name: String,
    pub actual_rpm: f32,
    pub min_rpm: f32,
    pub max_rpm: f32,
    pub estimated_power_w: f32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TempSensor {
    pub key: String,
    pub category: String,
    pub value_celsius: f32,
    /// True when the value is from a previous sample (sensor read failed this cycle).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProcessPower {
    pub pid: i32,
    pub name: String,
    pub power_w: f32,
    pub energy_mj: f64,
    pub alive: bool,
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    pub phys_mem_bytes: u64,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
}


/// Detects IOReport Energy Model counters that have stopped updating.
///
/// CPU, DRAM and fabric power can never all be exactly 0 W on a running Mac,
/// so zero for `STALL_AFTER_MS` of consecutive samples means the counters are
/// stale rather than the machine being idle.
#[derive(Debug, Default)]
pub struct StallDetector {
    zero_ms: u64,
}

impl StallDetector {
    pub const STALL_AFTER_MS: u64 = 4000;

    /// Feed one delta sample covering `dt_ms`; returns true while stalled.
    pub fn update(&mut self, soc: &SocPower, dt_ms: u64) -> bool {
        if soc.cpu_w <= 0.0 && soc.dram_w <= 0.0 && soc.fabric_w <= 0.0 {
            self.zero_ms = self.zero_ms.saturating_add(dt_ms);
        } else {
            self.zero_ms = 0;
        }
        self.zero_ms >= Self::STALL_AFTER_MS
    }
}

#[cfg(test)]
mod stall_tests {
    use super::*;

    fn soc(cpu: f32, dram: f32, fabric: f32) -> SocPower {
        SocPower {
            cpu_w: cpu,
            dram_w: dram,
            fabric_w: fabric,
            ..Default::default()
        }
    }

    #[test]
    fn flags_only_after_sustained_zero() {
        let mut d = StallDetector::default();
        assert!(!d.update(&soc(0.0, 0.0, 0.0), 250));
        for _ in 0..14 {
            assert!(!d.update(&soc(0.0, 0.0, 0.0), 250));
        }
        assert!(d.update(&soc(0.0, 0.0, 0.0), 250)); // 16 x 250 ms = 4 s
    }

    #[test]
    fn any_live_reading_resets() {
        let mut d = StallDetector::default();
        for _ in 0..20 {
            d.update(&soc(0.0, 0.0, 0.0), 250);
        }
        assert!(!d.update(&soc(0.4, 0.0, 0.0), 250));
        assert!(!d.update(&soc(0.0, 0.0, 0.0), 250));
    }

    #[test]
    fn healthy_machine_never_flags() {
        let mut d = StallDetector::default();
        for _ in 0..100 {
            assert!(!d.update(&soc(0.4, 0.1, 0.2), 250));
        }
    }
}
