use clap::Parser;
use serde::Serialize;

#[derive(Parser, Debug)]
#[command(name = env!("CARGO_PKG_NAME"), version, about = "Apple Silicon Power Monitor TUI")]
pub struct CliArgs {
    /// Sampling interval in milliseconds
    #[arg(long, default_value_t = 500)]
    pub interval: u64,

    /// Output JSON to stdout instead of TUI
    #[arg(long)]
    pub json: bool,

    /// Dump all IOReport channel names and exit (for diagnostics)
    #[arg(long)]
    pub dump: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Metrics {
    pub soc: SocPower,
    pub battery: BatteryInfo,
    pub display: DisplayInfo,
    pub keyboard: KeyboardInfo,
    pub audio: AudioInfo,
    pub network: NetworkInfo,
    pub disk: DiskInfo,
    pub ssd_power_w: f32,
    pub usb_devices: Vec<UsbDevice>,
    pub wifi: WifiInfo,
    pub bluetooth_devices: Vec<BluetoothDevice>,
    pub bluetooth_power_w: f32,
    pub power_assertions: Vec<PowerAssertion>,
    pub top_processes: Vec<ProcessPower>,
    pub all_procs_power_w: f32,
    pub all_procs_energy_mj: f64,
    pub fans: Vec<FanInfo>,
    pub temperatures: Vec<TempSensor>,
    pub sys_power_w: f32,
    pub gpu_cores: u32,
    pub dram_gb: u32,
    pub mem_used_gb: f32,
    pub cpu_usage_pct: Vec<f32>,
    pub ssd_model: String,
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
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DisplayInfo {
    pub brightness_pct: f32,
    pub nits: f32,
    pub estimated_power_w: f32,
    pub available: bool,
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
pub struct DiskInfo {
    pub read_bytes_per_sec: f64,
    pub write_bytes_per_sec: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UsbDevice {
    pub name: String,
    pub vendor_id: u32,
    pub product_id: u32,
    pub power_ma: Option<u32>,
    pub speed: u32,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WifiInfo {
    pub connected: bool,
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
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProcessPower {
    pub pid: i32,
    pub name: String,
    pub power_w: f32,
    pub energy_mj: f64,
    pub alive: bool,
}
