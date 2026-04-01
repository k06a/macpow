use macpow::sma::TimeSma;
use macpow::types::*;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

// ── Styles ───────────────────────────────────────────────────────────────────

const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const DIM: Style = Style::new().fg(Color::DarkGray);
const DATA_STYLE: Style = Style::new().fg(Color::Rgb(80, 140, 255));
const PENDING: Style = Style::new().fg(Color::Magenta);
#[allow(dead_code)]
const CURSOR_BG: Style = Style::new().bg(Color::Rgb(50, 50, 60));
const TREE_STYLE: Style = Style::new().fg(Color::Reset);
const PIN_MARKER: &str = "▸ ";
const HISTORY_LEN: usize = 240;
const CHART_HEIGHT: u16 = 7;

const COL_FREQ: u16 = 10;
const COL_TEMP: u16 = 16;
const COL_CUR: u16 = 14;
const COL_TOT: u16 = 14;

const SPARK_CHARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '▇'];
const BAR_EIGHTHS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn power_color(w: f32) -> Color {
    match w {
        w if w < 1.0 => Color::Rgb(46, 139, 87),
        w if w < 5.0 => Color::Rgb(220, 180, 0), // golden yellow
        w if w < 10.0 => Color::Rgb(255, 140, 0), // carrot orange
        _ => Color::Rgb(255, 50, 50),            // bright red
    }
}

fn fmt_wh(wh: f64) -> String {
    let mwh = wh * 1000.0;
    if mwh.abs() >= 1000.0 {
        format!("{:>10.3} Wh", wh)
    } else {
        format!("{:>10.2} mWh", mwh)
    }
}

#[allow(dead_code)]
fn fmt_mwh(mwh: f64) -> String {
    if mwh.abs() >= 1000.0 {
        format!("{:>10.3} Wh", mwh / 1000.0)
    } else {
        format!("{:>10.2} mWh", mwh)
    }
}

fn human_rate(bps: f64) -> String {
    match bps {
        b if b < 1024.0 => format!("{:>7.0} B/s", b),
        b if b < 1024.0 * 1024.0 => format!("{:>7.1} KB/s", b / 1024.0),
        b => format!("{:>7.1} MB/s", b / (1024.0 * 1024.0)),
    }
}

fn human_bytes(b: f64) -> String {
    match b {
        b if b < 1024.0 => format!("{:.0} B", b),
        b if b < 1024.0 * 1024.0 => format!("{:.1} KB", b / 1024.0),
        b if b < 1024.0 * 1024.0 * 1024.0 => format!("{:.1} MB", b / (1024.0 * 1024.0)),
        b => format!("{:.2} GB", b / (1024.0 * 1024.0 * 1024.0)),
    }
}

fn fmt_freq(mhz: f32) -> String {
    if mhz > 0.0 {
        format!("{:.0} MHz", mhz)
    } else {
        String::new()
    }
}

// ── TreeRow: pure data for one row ──────────────────────────────────────────

struct TreeRow {
    prefix: String,
    label: String,
    freq: String,
    temp: String,
    current: String,
    total: String,
    label_style: Style,
    current_style: Style,
    key: Option<&'static str>,
    parent: Option<&'static str>,
    pinned: bool,
    #[allow(dead_code)]
    is_header: bool,
}

impl TreeRow {
    #[allow(clippy::too_many_arguments)]
    fn pw(
        key: &'static str,
        parent: Option<&'static str>,
        prefix: &str,
        label: &str,
        watts: f32,
        wh: f64,
        style: Style,
        pinned: bool,
    ) -> Self {
        Self::pw_inner(
            key, parent, prefix, label, watts, wh, "", "", style, pinned, false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn pw_est(
        key: &'static str,
        parent: Option<&'static str>,
        prefix: &str,
        label: &str,
        watts: f32,
        wh: f64,
        style: Style,
        pinned: bool,
    ) -> Self {
        Self::pw_inner(
            key, parent, prefix, label, watts, wh, "", "", style, pinned, true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn pw_full(
        key: &'static str,
        parent: Option<&'static str>,
        prefix: &str,
        label: &str,
        watts: f32,
        wh: f64,
        freq: &str,
        temp: &str,
        style: Style,
        pinned: bool,
    ) -> Self {
        Self::pw_inner(
            key, parent, prefix, label, watts, wh, freq, temp, style, pinned, false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn pw_full_est(
        key: &'static str,
        parent: Option<&'static str>,
        prefix: &str,
        label: &str,
        watts: f32,
        wh: f64,
        freq: &str,
        temp: &str,
        style: Style,
        pinned: bool,
    ) -> Self {
        Self::pw_inner(
            key, parent, prefix, label, watts, wh, freq, temp, style, pinned, true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn pw_inner(
        key: &'static str,
        parent: Option<&'static str>,
        prefix: &str,
        label: &str,
        watts: f32,
        wh: f64,
        freq: &str,
        temp: &str,
        style: Style,
        pinned: bool,
        estimated: bool,
    ) -> Self {
        let w = watts + 0.0;
        let current = if estimated {
            format!("≈{:.3} W", w)
        } else {
            format!("{:>7.3} W", w)
        };
        let total = if estimated {
            let s = fmt_wh(wh);
            format!("≈{}", s.trim_start())
        } else {
            fmt_wh(wh)
        };
        Self {
            prefix: prefix.to_string(),
            label: label.to_string(),
            freq: freq.to_string(),
            temp: temp.to_string(),
            current,
            total,
            label_style: style.fg(style.fg.unwrap_or(power_color(w.abs()))),
            current_style: Style::default().fg(power_color(w.abs())),
            key: Some(key),
            parent,
            is_header: false,
            pinned,
        }
    }

    fn info(
        parent: Option<&'static str>,
        prefix: &str,
        label: &str,
        current: &str,
        total: &str,
        style: Style,
    ) -> Self {
        Self {
            prefix: prefix.to_string(),
            label: label.to_string(),
            freq: String::new(),
            temp: String::new(),
            current: current.to_string(),
            total: total.to_string(),
            label_style: style,
            current_style: style,
            key: None,
            parent,
            is_header: false,
            pinned: false,
        }
    }

    fn has_children_in(&self, rows: &[TreeRow]) -> bool {
        self.key
            .map(|k| rows.iter().any(|r| r.parent == Some(k)))
            .unwrap_or(false)
    }

    fn separator() -> Self {
        Self {
            prefix: String::new(),
            label: "\x00sep".into(), // sentinel for full-width separator
            freq: String::new(),
            temp: String::new(),
            current: String::new(),
            total: String::new(),
            label_style: DIM,
            current_style: DIM,
            key: None,
            parent: None,
            pinned: false,
            is_header: false,
        }
    }
}

// ── Cumulative energy tracker ────────────────────────────────────────────────

#[derive(Default)]
struct Wh {
    ecpu: f64,
    pcpu: f64,
    cpu: f64,
    gpu: f64,
    ane: f64,
    dram: f64,
    gpu_sram: f64,
    isp: f64,
    display_soc: f64,
    display_ext: f64,
    pcie: f64,
    media: f64,
    fabric: f64,
    ssd: f64,
    display: f64,
    backlight: f64,
    keyboard: f64,
    audio: f64,
    fans: f64,
    wifi: f64,
    bluetooth: f64,
    sys: f64,
    battery: f64,
    adapter: f64,
    net_down_bytes: f64,
    net_up_bytes: f64,
    eth_down_bytes: f64,
    eth_up_bytes: f64,
    wifi_down_bytes: f64,
    wifi_up_bytes: f64,
    disk_read_bytes: f64,
    disk_write_bytes: f64,
}

// ── SMA bank ─────────────────────────────────────────────────────────────────

macro_rules! sma_fields {
    ($($f:ident),*) => {
        struct MetricsSma { $( $f: TimeSma, )* }
        impl MetricsSma {
            fn new(w: f64) -> Self { Self { $( $f: TimeSma::new(w), )* } }
            fn set_all_windows(&mut self, s: f64) { $( self.$f.set_window(s); )* }
            fn clear_all(&mut self) { $( self.$f.clear(); )* }
        }
    }
}

sma_fields!(
    soc_total,
    cpu,
    ecpu,
    pcpu,
    gpu,
    ane,
    dram,
    gpu_sram,
    isp,
    display_soc,
    display_ext,
    pcie,
    media,
    fabric,
    ssd,
    display,
    backlight,
    keyboard,
    audio,
    fan_total,
    wifi,
    bluetooth,
    sys,
    battery,
    adapter,
    net_down,
    net_up,
    eth_down,
    eth_up,
    wifi_down,
    wifi_up,
    ecpu_freq,
    pcpu_freq,
    gpu_freq
);

impl MetricsSma {
    fn push_metrics(&mut self, m: &Metrics) {
        self.soc_total.push(m.soc.total_w);
        self.cpu.push(m.soc.cpu_w);
        self.ecpu.push(m.soc.ecpu_total_w());
        self.pcpu.push(m.soc.pcpu_total_w());
        self.gpu.push(m.soc.gpu_w);
        self.ane.push(m.soc.ane_w);
        self.dram.push(m.soc.dram_w);
        self.gpu_sram.push(m.soc.gpu_sram_w);
        self.isp.push(m.soc.isp_w);
        self.display_soc
            .push(m.soc.display_soc_w + m.soc.display_ext_w);
        self.display_ext.push(m.soc.display_ext_w);
        self.pcie.push(m.soc.pcie_w);
        self.media.push(m.soc.media_w);
        self.fabric.push(m.soc.fabric_w);
        self.ssd.push(m.ssd_power_w);
        self.display.push(m.display.estimated_power_w);
        self.backlight.push(if m.backlight_power_w > 0.0 {
            m.backlight_power_w
        } else {
            m.display.estimated_power_w
        });
        self.keyboard.push(m.keyboard.estimated_power_w);
        self.audio.push(m.audio.estimated_power_w);
        self.fan_total
            .push(m.fans.iter().map(|f| f.estimated_power_w).sum());
        self.wifi.push(if m.wifi_power_w > 0.0 {
            m.wifi_power_w
        } else {
            m.wifi.estimated_power_w
        });
        self.bluetooth.push(m.bluetooth_power_w);
        self.sys.push(m.sys_power_w);
        self.battery.push(m.battery.drain_w as f32);
        let adapter_draw = if m.adapter.connected {
            self.soc_total.get()
                + self.ssd.get()
                + self.display.get()
                + self.display_soc.get()
                + self.display_ext.get()
                + self.keyboard.get()
                + self.audio.get()
                + self.fan_total.get()
                + self.wifi.get()
                + self.bluetooth.get()
                + self.pcie.get()
                + (m.battery.drain_w as f32).abs()
        } else {
            0.0
        };
        self.adapter.push(adapter_draw);
        self.net_down.push(m.network.bytes_in_per_sec as f32);
        self.net_up.push(m.network.bytes_out_per_sec as f32);
        self.eth_down.push(m.eth_network.bytes_in_per_sec as f32);
        self.eth_up.push(m.eth_network.bytes_out_per_sec as f32);
        self.wifi_down.push(m.wifi_network.bytes_in_per_sec as f32);
        self.wifi_up.push(m.wifi_network.bytes_out_per_sec as f32);
        self.ecpu_freq.push(m.soc.ecpu_freq_mhz as f32);
        self.pcpu_freq.push(m.soc.pcpu_freq_mhz as f32);
        self.gpu_freq.push(m.soc.gpu_freq_mhz as f32);
    }
}

// ── App ──────────────────────────────────────────────────────────────────────

pub struct App {
    pub metrics: Metrics,
    pub cursor: usize,
    last_tick: Option<Instant>,
    wh: Wh,
    sma: MetricsSma,
    pub sma_window: u32,
    pub latency_ms: u64,
    temp_min: BTreeMap<String, f32>,
    temp_max: BTreeMap<String, f32>,
    temp_sum: BTreeMap<String, f64>,
    temp_count: BTreeMap<String, u64>,
    // Keys accumulate per PID for sparkline history; bounded by HISTORY_LEN per key.
    // Dead process keys remain so pinned charts keep rendering.
    history: BTreeMap<&'static str, VecDeque<f64>>,
    pinned: Vec<&'static str>,
    collapsed: std::collections::HashSet<&'static str>,
    total_rows: usize,
    row_keys_cache: Vec<Option<&'static str>>,
    row_parents_cache: Vec<Option<&'static str>>,
    row_is_sep: Vec<bool>,
    // Insert-only: labels are kept for chart titles of dead/pinned processes.
    labels: BTreeMap<&'static str, String>,
    proc_baseline: std::collections::HashMap<i32, f64>,
    proc_keys: std::collections::HashMap<i32, &'static str>,
    fan_wh: Vec<f64>,
    usb_wh: Vec<f64>,
    usb_prev_bytes: Vec<(u64, u64)>,
    usb_rates: Vec<(f64, f64)>,
    machine_name: String,
    tree_data_y: u16,
    tree_scroll: usize,
    tree_vis_h: usize,
    term_height: u16,
}

impl App {
    pub fn new() -> Self {
        let machine_name = read_machine_name();
        Self {
            metrics: Metrics::default(),
            cursor: 2, // start at tree root (after battery + separator)
            last_tick: None,
            wh: Wh::default(),
            sma: MetricsSma::new(0.0),
            sma_window: 0,
            latency_ms: 250,
            temp_min: BTreeMap::new(),
            temp_max: BTreeMap::new(),
            temp_sum: BTreeMap::new(),
            temp_count: BTreeMap::new(),
            history: BTreeMap::new(),
            pinned: Vec::new(),
            collapsed: [
                "wifi",
                "bluetooth",
                "ssd",
                "ecpu",
                "pcpu",
                "gpu",
                "ane",
                "display",
                "fabric",
                "usb0",
                "usb1",
                "usb2",
                "usb3",
                "usb4",
                "usb5",
                "usb6",
                "usb7",
            ]
            .into_iter()
            .chain(["ethernet", "ssd_nand", "trackpad"])
            .collect(),
            total_rows: 0,
            row_keys_cache: Vec::new(),
            row_parents_cache: Vec::new(),
            row_is_sep: Vec::new(),
            labels: BTreeMap::new(),
            proc_baseline: std::collections::HashMap::new(),
            proc_keys: std::collections::HashMap::new(),
            fan_wh: Vec::new(),
            usb_wh: Vec::new(),
            usb_prev_bytes: Vec::new(),
            usb_rates: Vec::new(),
            machine_name,
            tree_data_y: 0,
            tree_scroll: 0,
            tree_vis_h: 0,
            term_height: 40,
        }
    }

    fn push_history(&mut self, key: &'static str, val: f64) {
        let buf = self
            .history
            .entry(key)
            .or_insert_with(|| VecDeque::with_capacity(HISTORY_LEN + 1));
        buf.push_back(val);
        if buf.len() > HISTORY_LEN {
            buf.pop_front();
        }
    }

    pub fn update(&mut self, m: Metrics) {
        self.sma.push_metrics(&m);

        if let Some(prev) = self.last_tick {
            let dt_h = prev.elapsed().as_secs_f64() / 3600.0;
            let dt_s = prev.elapsed().as_secs_f64();
            self.wh.ecpu += m.soc.ecpu_total_w() as f64 * dt_h;
            self.wh.pcpu += m.soc.pcpu_total_w() as f64 * dt_h;
            self.wh.cpu += m.soc.cpu_w as f64 * dt_h;
            self.wh.gpu += m.soc.gpu_w as f64 * dt_h;
            self.wh.ane += m.soc.ane_w as f64 * dt_h;
            self.wh.dram += m.soc.dram_w as f64 * dt_h;
            self.wh.gpu_sram += m.soc.gpu_sram_w as f64 * dt_h;
            self.wh.isp += m.soc.isp_w as f64 * dt_h;
            self.wh.display_soc += m.soc.display_soc_w as f64 * dt_h;
            self.wh.display_ext += m.soc.display_ext_w as f64 * dt_h;
            self.wh.pcie += m.soc.pcie_w as f64 * dt_h;
            self.wh.media += m.soc.media_w as f64 * dt_h;
            self.wh.fabric += m.soc.fabric_w as f64 * dt_h;
            self.wh.ssd += m.ssd_power_w as f64 * dt_h;
            self.wh.display += m.display.estimated_power_w as f64 * dt_h;
            self.wh.backlight += if m.backlight_power_w > 0.0 {
                m.backlight_power_w as f64 * dt_h
            } else {
                m.display.estimated_power_w as f64 * dt_h
            };
            self.wh.keyboard += m.keyboard.estimated_power_w as f64 * dt_h;
            self.wh.audio += m.audio.estimated_power_w as f64 * dt_h;
            self.wh.fans += m
                .fans
                .iter()
                .map(|f| f.estimated_power_w as f64)
                .sum::<f64>()
                * dt_h;
            // Per-fan Wh
            self.fan_wh.resize(m.fans.len(), 0.0);
            for (i, fan) in m.fans.iter().enumerate() {
                self.fan_wh[i] += fan.estimated_power_w as f64 * dt_h;
            }
            // Per-USB device Wh + data rates
            self.usb_wh.resize(m.usb_devices.len(), 0.0);
            self.usb_rates.resize(m.usb_devices.len(), (0.0, 0.0));
            self.usb_prev_bytes.resize(m.usb_devices.len(), (0, 0));
            for (i, d) in m.usb_devices.iter().enumerate() {
                let watts = d.power_ma.unwrap_or(0) as f64 * 5.0 / 1000.0;
                self.usb_wh[i] += watts * dt_h;
                let (prev_r, prev_w) = self.usb_prev_bytes[i];
                if (prev_r > 0 || prev_w > 0) && dt_s > 0.001 {
                    self.usb_rates[i] = (
                        d.bytes_read.saturating_sub(prev_r) as f64 / dt_s,
                        d.bytes_written.saturating_sub(prev_w) as f64 / dt_s,
                    );
                }
                self.usb_prev_bytes[i] = (d.bytes_read, d.bytes_written);
            }
            self.wh.wifi += if m.wifi_power_w > 0.0 {
                m.wifi_power_w as f64 * dt_h
            } else {
                m.wifi.estimated_power_w as f64 * dt_h
            };
            self.wh.bluetooth += m.bluetooth_power_w as f64 * dt_h;
            self.wh.sys += m.sys_power_w as f64 * dt_h;
            self.wh.battery += m.battery.drain_w * dt_h;
            if m.adapter.connected {
                let adapter_w = if m.adapter_power_w > 0.0 {
                    m.adapter_power_w as f64
                } else {
                    self.sma.adapter.get() as f64
                };
                self.wh.adapter += adapter_w * dt_h;
            }
            self.wh.net_down_bytes += m.network.bytes_in_per_sec * dt_s;
            self.wh.net_up_bytes += m.network.bytes_out_per_sec * dt_s;
            self.wh.eth_down_bytes += m.eth_network.bytes_in_per_sec * dt_s;
            self.wh.eth_up_bytes += m.eth_network.bytes_out_per_sec * dt_s;
            self.wh.wifi_down_bytes += m.wifi_network.bytes_in_per_sec * dt_s;
            self.wh.wifi_up_bytes += m.wifi_network.bytes_out_per_sec * dt_s;
            self.wh.disk_read_bytes += m.disk.read_bytes_per_sec * dt_s;
            self.wh.disk_write_bytes += m.disk.write_bytes_per_sec * dt_s;
        }
        self.last_tick = Some(Instant::now());

        self.push_history("soc", m.soc.total_w as f64);
        self.push_history("cpu", m.soc.cpu_w as f64);
        self.push_history("ecpu", m.soc.ecpu_total_w() as f64);
        for (ci, core) in m
            .soc
            .ecpu_clusters
            .iter()
            .flat_map(|cl| cl.cores.iter())
            .enumerate()
        {
            let key = proc_key(&mut self.proc_keys, -(ci as i32 + 1000));
            self.push_history(key, core.watts as f64);
        }
        self.push_history("pcpu", m.soc.pcpu_total_w() as f64);
        for (ci, core) in m.soc.pcpu_cluster.cores.iter().enumerate() {
            let key = proc_key(&mut self.proc_keys, -(ci as i32 + 2000));
            self.push_history(key, core.watts as f64);
        }
        self.push_history("gpu", m.soc.gpu_w as f64);
        self.push_history("ane", m.soc.ane_w as f64);
        self.push_history("dram", m.soc.dram_w as f64);
        self.push_history("gpu_sram", m.soc.gpu_sram_w as f64);
        self.push_history("isp", m.soc.isp_w as f64);
        self.push_history(
            "display_soc",
            (m.soc.display_soc_w + m.soc.display_ext_w) as f64,
        );
        self.push_history("display_ext", m.soc.display_ext_w as f64);
        self.push_history("pcie", m.soc.pcie_w as f64);
        self.push_history("media", m.soc.media_w as f64);
        self.push_history("fabric", m.soc.fabric_w as f64);
        self.push_history("ssd", m.ssd_power_w as f64);
        self.push_history("display", m.display.estimated_power_w as f64);
        self.push_history(
            "backlight",
            if m.backlight_power_w > 0.0 {
                m.backlight_power_w as f64
            } else {
                m.display.estimated_power_w as f64
            },
        );
        self.push_history("keyboard", m.keyboard.estimated_power_w as f64);
        self.push_history("audio", m.audio.estimated_power_w as f64);
        self.push_history(
            "fans",
            m.fans.iter().map(|f| f.estimated_power_w as f64).sum(),
        );
        for (i, fan) in m.fans.iter().enumerate() {
            self.push_history(fan_key(i), fan.estimated_power_w as f64);
        }
        for (i, d) in m.usb_devices.iter().enumerate() {
            self.push_history(usb_key(i), d.power_ma.unwrap_or(0) as f64 * 5.0 / 1000.0);
        }
        self.push_history(
            "wifi",
            if m.wifi_power_w > 0.0 {
                m.wifi_power_w as f64
            } else {
                m.wifi.estimated_power_w as f64
            },
        );
        self.push_history("bluetooth", m.bluetooth_power_w as f64);
        self.push_history("eth_down", m.eth_network.bytes_in_per_sec);
        self.push_history("eth_up", m.eth_network.bytes_out_per_sec);
        self.push_history("wifi_down", m.wifi_network.bytes_in_per_sec);
        self.push_history("wifi_up", m.wifi_network.bytes_out_per_sec);
        self.push_history("disk_read", m.disk.read_bytes_per_sec);
        self.push_history("disk_write", m.disk.write_bytes_per_sec);
        self.push_history(
            "peripherals",
            (m.wifi.estimated_power_w + m.bluetooth_power_w) as f64,
        );
        self.push_history("system", m.sys_power_w as f64);
        self.push_history("battery", m.battery.drain_w.abs());
        self.push_history(
            "adapter",
            if m.adapter_power_w > 0.0 {
                m.adapter_power_w as f64
            } else {
                self.sma.adapter.get() as f64
            },
        );
        self.push_history("software", m.all_procs_power_w as f64);
        for p in &m.top_processes {
            let key = proc_key(&mut self.proc_keys, p.pid);
            self.push_history(key, p.power_w as f64);
        }

        let cat_now = temps_by_category(&m.temperatures);
        for (cat, vals) in &cat_now {
            let (avg, min, max) = stats(vals);
            let e_min = self.temp_min.entry(cat.clone()).or_insert(f32::INFINITY);
            *e_min = e_min.min(min);
            let e_max = self
                .temp_max
                .entry(cat.clone())
                .or_insert(f32::NEG_INFINITY);
            *e_max = e_max.max(max);
            *self.temp_sum.entry(cat.clone()).or_insert(0.0) += avg as f64;
            *self.temp_count.entry(cat.clone()).or_insert(0) += 1;
        }

        self.metrics = m;
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::KeyCode;

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('c')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                return true
            }
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('h') | KeyCode::Left => self.collapse_or_parent(),
            KeyCode::Right => self.expand_or_child(),
            KeyCode::Char('r') => self.reset(),
            KeyCode::Char('a') => self.cycle_sma(),
            KeyCode::Char('l') => self.cycle_latency(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::PageUp => self.move_cursor(-10),
            KeyCode::PageDown => self.move_cursor(10),
            KeyCode::Char(' ') => self.toggle_pin(),
            KeyCode::Char('-') => self.collapse_all(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.expand_all(),
            _ => {}
        }
        false
    }

    fn move_cursor(&mut self, delta: i32) {
        let max = self.total_rows.saturating_sub(1) as i32;
        let mut pos = (self.cursor as i32 + delta).clamp(0, max);
        let dir = if delta >= 0 { 1 } else { -1 };
        // Skip separator rows
        while pos >= 0 && pos <= max && self.row_is_sep.get(pos as usize).copied().unwrap_or(false)
        {
            pos += dir;
        }
        self.cursor = pos.clamp(0, max) as usize;
    }

    pub fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let y = mouse.row;
                if y >= self.tree_data_y {
                    let vi = (y - self.tree_data_y) as usize;
                    if vi < self.tree_vis_h {
                        let target = self.tree_scroll + vi;
                        if target < self.total_rows
                            && !self.row_is_sep.get(target).copied().unwrap_or(false)
                        {
                            if target == self.cursor {
                                if let Some(Some(key)) = self.row_keys_cache.get(self.cursor) {
                                    if self.collapsed.contains(key) {
                                        self.collapsed.remove(key);
                                    } else if self.row_parents_cache.contains(&Some(*key)) {
                                        self.collapsed.insert(*key);
                                    }
                                }
                            } else {
                                self.cursor = target;
                            }
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                self.cursor = self.cursor.saturating_sub(3);
            }
            MouseEventKind::ScrollDown => {
                self.cursor = (self.cursor + 3).min(self.total_rows.saturating_sub(1));
            }
            _ => {}
        }
    }

    fn toggle_pin(&mut self) {
        if let Some(Some(key)) = self.row_keys_cache.get(self.cursor) {
            if let Some(pos) = self.pinned.iter().position(|&k| k == *key) {
                self.pinned.remove(pos);
            } else {
                self.pinned.push(*key);
            }
        }
    }

    fn collapse_or_parent(&mut self) {
        if let Some(Some(key)) = self.row_keys_cache.get(self.cursor) {
            if !self.collapsed.contains(key) {
                // Check if this node has children (is a parent)
                if self.row_parents_cache.contains(&Some(*key)) {
                    self.collapsed.insert(*key);
                    return;
                }
            }
        }
        // Move to parent
        if let Some(Some(parent)) = self.row_parents_cache.get(self.cursor) {
            if let Some(pos) = self.row_keys_cache.iter().position(|k| *k == Some(*parent)) {
                self.cursor = pos;
            }
        }
    }

    fn expand_or_child(&mut self) {
        if let Some(Some(key)) = self.row_keys_cache.get(self.cursor) {
            if self.collapsed.remove(key) {
                return;
            }
            // Move to first child
            if let Some(pos) = self
                .row_parents_cache
                .iter()
                .enumerate()
                .skip(self.cursor + 1)
                .find(|(_, p)| **p == Some(*key))
                .map(|(i, _)| i)
            {
                self.cursor = pos;
            }
        }
    }

    fn collapse_all(&mut self) {
        for k in self.row_keys_cache.iter().flatten() {
            if self.row_parents_cache.contains(&Some(*k)) {
                self.collapsed.insert(*k);
            }
        }
    }

    fn expand_all(&mut self) {
        self.collapsed.clear();
    }

    fn reset(&mut self) {
        self.wh = Wh::default();
        self.sma.clear_all();
        self.temp_min.clear();
        self.temp_max.clear();
        self.temp_sum.clear();
        self.temp_count.clear();
        self.history.clear();
        self.fan_wh.iter_mut().for_each(|v| *v = 0.0);
        self.usb_wh.iter_mut().for_each(|v| *v = 0.0);
        self.usb_prev_bytes.clear();
        self.usb_rates.clear();
        self.proc_baseline = self
            .metrics
            .top_processes
            .iter()
            .map(|p| (p.pid, p.energy_mj))
            .collect();
    }

    fn cycle_sma(&mut self) {
        self.sma_window = match self.sma_window {
            0 => 5,
            5 => 10,
            _ => 0,
        };
        self.sma.set_all_windows(self.sma_window as f64);
    }

    fn cycle_latency(&mut self) {
        self.latency_ms = match self.latency_ms {
            250 => 1000,
            1000 => 2000,
            _ => 250,
        };
    }

    pub fn poll_interval_ms(&self) -> u64 {
        self.latency_ms
    }

    pub fn draw(&mut self, f: &mut Frame) {
        self.term_height = f.area().height;
        let all_rows = self.build_rows();

        // Filter out children of collapsed nodes
        let rows: Vec<&TreeRow> = all_rows
            .iter()
            .filter(|r| !self.is_hidden(r, &all_rows))
            .collect();

        self.total_rows = rows.len();

        // Preserve cursor position by tracking the selected resource key
        let prev_key = self.row_keys_cache.get(self.cursor).copied().flatten();
        self.row_keys_cache = rows.iter().map(|r| r.key).collect();
        self.row_parents_cache = rows.iter().map(|r| r.parent).collect();
        self.row_is_sep = rows.iter().map(|r| r.label == "\x00sep").collect();

        // Restore cursor to the same key if rows shifted
        if let Some(pk) = prev_key {
            if let Some(pos) = self.row_keys_cache.iter().position(|k| *k == Some(pk)) {
                self.cursor = pos;
            }
        }
        self.cursor = self.cursor.min(self.total_rows.saturating_sub(1));

        // Cache labels for chart titles
        for r in &rows {
            if let Some(key) = r.key {
                if !r.label.is_empty() {
                    self.labels.insert(key, r.label.clone());
                }
            }
        }

        let cursor_key = self
            .row_keys_cache
            .get(self.cursor)
            .copied()
            .flatten()
            .or_else(|| {
                // Fall back to parent's key for rows without their own key
                self.row_parents_cache.get(self.cursor).copied().flatten()
            });
        let chart_keys = self.chart_keys(cursor_key);
        let chart_count = if chart_keys.is_empty() {
            0
        } else if self.pinned.is_empty() {
            1
        } else {
            self.pinned.len().max(chart_keys.len())
        };
        let chart_h = chart_count as u16 * CHART_HEIGHT;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(chart_h),
                Constraint::Length(1),
            ])
            .split(f.area());

        self.draw_tree_buf(f, chunks[0], &rows, &all_rows);
        if !chart_keys.is_empty() {
            self.draw_charts(f, chunks[1], &chart_keys);
        }
        self.draw_footer(f, chunks[2]);
    }

    fn is_hidden(&self, row: &TreeRow, all: &[TreeRow]) -> bool {
        let mut parent = row.parent;
        while let Some(p) = parent {
            if self.collapsed.contains(p) {
                return true;
            }
            parent = all.iter().find(|r| r.key == Some(p)).and_then(|r| r.parent);
        }
        false
    }

    fn chart_keys(&self, cursor_key: Option<&'static str>) -> Vec<&'static str> {
        let mut keys: Vec<&'static str> = Vec::new();
        if let Some(ck) = cursor_key {
            if !self.pinned.contains(&ck) {
                keys.push(ck);
            }
        }
        // Pinned in reverse order: last pinned on top, first pinned at bottom
        for &pk in self.pinned.iter().rev() {
            keys.push(pk);
        }
        keys
    }

    // ── Build rows ──────────────────────────────────────────────────────────

    fn build_rows(&mut self) -> Vec<TreeRow> {
        let m = &self.metrics;
        let w = &self.wh;
        let s = &self.sma;
        let pin = |key: &str| -> bool { self.pinned.contains(&key) };

        let e_count: usize = m.soc.ecpu_clusters.iter().map(|c| c.cores.len()).sum();
        let p_count = m.soc.pcpu_cluster.cores.len();
        let (e_temps, p_temps) = selected_cpu_core_temps(&m.temperatures, e_count, p_count);
        let mut temp_groups = temps_by_category(&m.temperatures);
        let cpu_display_temps: Vec<f32> = e_temps.iter().chain(p_temps.iter()).copied().collect();
        if !cpu_display_temps.is_empty() {
            temp_groups.insert("CPU".to_string(), cpu_display_temps);
        }
        let temps_pending = m.temperatures.is_empty();
        let temp_info = |cat: &str| -> String {
            temp_groups
                .get(cat)
                .map(|v| {
                    let avg = v.iter().sum::<f32>() / v.len() as f32;
                    let cur_min = v.iter().copied().fold(f32::INFINITY, f32::min);
                    let cur_max = v.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                    format!("{:.0}°C ({:.0}–{:.0})", avg, cur_min, cur_max)
                })
                .unwrap_or_else(|| {
                    if temps_pending {
                        "pending…".into()
                    } else {
                        String::new()
                    }
                })
        };

        let mut rows: Vec<TreeRow> = Vec::new();

        let last_section = { "peripherals" };
        let t = |section: &str| -> String {
            if section == last_section {
                "└─ ".into()
            } else {
                "├─ ".into()
            }
        };
        let c = |section: &str| -> String {
            if section == last_section {
                "   ".into()
            } else {
                "│  ".into()
            }
        };

        rows.push(TreeRow::separator());

        // ── Power Adapter (first row of the battery section)
        if m.adapter.connected {
            let has_pdtr = m.adapter_power_w > 0.0;
            let adapter_w = if has_pdtr {
                m.adapter_power_w
            } else {
                s.adapter.get()
            };
            let adapter_label = if m.adapter.is_wireless {
                format!("Power Adapter {}W (wireless)", m.adapter.watts)
            } else {
                format!(
                    "Power Adapter {}W ({:.1}V × {:.1}A)",
                    m.adapter.watts,
                    m.adapter.voltage_mv as f64 / 1000.0,
                    m.adapter.current_ma as f64 / 1000.0
                )
            };
            if has_pdtr {
                rows.push(TreeRow::pw(
                    "adapter",
                    None,
                    "",
                    &adapter_label,
                    adapter_w,
                    w.adapter,
                    Style::default().fg(Color::Rgb(46, 139, 87)),
                    pin("adapter"),
                ));
            } else {
                rows.push(TreeRow::pw_est(
                    "adapter",
                    None,
                    "",
                    &adapter_label,
                    adapter_w,
                    w.adapter,
                    Style::default().fg(Color::Rgb(46, 139, 87)),
                    pin("adapter"),
                ));
            }
        }

        // ── Battery (standalone row before the tree)
        // Desktop Macs report a phantom battery (present=true, all values zero);
        // skip when max_capacity is 0 to avoid showing "Battery 0%".
        if m.battery.present && m.battery.max_capacity > 0 {
            let batt_w = s.battery.get();
            let t = m.battery.time_remaining_min;
            let has_time = t > 0;
            let effectively_charging = m.battery.external_connected && m.battery.drain_w < 0.0;
            let (display_w, charge_status, batt_style) = if m.battery.external_connected {
                // On external power: show charging power (positive)
                let status = if effectively_charging && has_time {
                    format!("full in {}h {:02}m", t / 60, t % 60)
                } else if effectively_charging {
                    "charging…".into()
                } else {
                    "on power".into()
                };
                (
                    batt_w.abs(),
                    status,
                    Style::default().fg(Color::Rgb(46, 139, 87)),
                )
            } else {
                // On battery: show drain power (negative)
                (
                    -batt_w.abs(),
                    if has_time {
                        format!("{}h {:02}m remaining", t / 60, t % 60)
                    } else {
                        "estimating…".into()
                    },
                    Style::default().fg(power_color(batt_w.abs())),
                )
            };
            let health_str = if m.battery.health_pct > 0.0 && m.battery.health_pct < 100.0 {
                format!(", health {:.0}%", m.battery.health_pct)
            } else {
                String::new()
            };
            let cycle_str = if m.battery.cycle_count > 0 {
                format!(", {} cycles", m.battery.cycle_count)
            } else {
                String::new()
            };
            let capacity_str = if m.battery.capacity_wh > 0.0 {
                format!(", {:.1} Wh", m.battery.capacity_wh)
            } else {
                String::new()
            };
            let batt_label = format!(
                "Battery {:.0}% ({}{}{}{})",
                m.battery.percent, charge_status, health_str, cycle_str, capacity_str,
            );
            rows.push(TreeRow::pw_full(
                "battery",
                None,
                "",
                &batt_label,
                display_w,
                w.battery,
                "",
                &temp_info("Battery"),
                batt_style,
                pin("battery"),
            ));
        }

        // ── BT devices with batteries (in the battery section)
        for d in &m.bluetooth_devices {
            if d.batteries.is_empty() {
                continue;
            }
            let bat = d
                .batteries
                .iter()
                .map(|(l, p)| {
                    if l.is_empty() {
                        p.clone()
                    } else {
                        format!("{}: {}", l, p)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            rows.push(TreeRow::info(
                None,
                "",
                &format!("{} {} [{}]", d.name, d.minor_type, bat),
                "",
                "",
                DIM,
            ));
        }

        rows.push(TreeRow::separator());

        // ── Root: machine name with system total (SMC PSTR)
        let sys_w = s.sys.get();
        let sys_wh = w.sys;
        rows.push(TreeRow::pw(
            "system",
            None,
            "",
            &self.machine_name,
            sys_w,
            sys_wh,
            BOLD,
            pin("system"),
        ));

        // ── SoC
        let soc_pending = m.soc.total_w == 0.0 && m.soc.ecpu_clusters.is_empty();
        if soc_pending {
            rows.push(TreeRow::pw(
                "soc",
                Some("system"),
                &t("soc"),
                "SoC (pending…)",
                0.0,
                0.0,
                PENDING,
                pin("soc"),
            ));
        } else {
            let soc_wh = w.cpu + w.gpu + w.ane + w.dram + w.gpu_sram + w.isp + w.media + w.fabric;
            let cp = c("soc");

            rows.push(TreeRow::pw(
                "soc",
                Some("system"),
                &t("soc"),
                "SoC",
                s.soc_total.get(),
                soc_wh,
                BOLD,
                pin("soc"),
            ));
            // Per-CPU usage from Mach API (first e_count are E-cores, next p_count are P-cores)
            // Mach API returns P-cores first (perflevel0), then E-cores (perflevel1)
            let cpu_usage = &m.cpu_usage_pct;
            let p_usage: Vec<f32> = cpu_usage.iter().take(p_count).copied().collect();
            let e_usage: Vec<f32> = cpu_usage
                .iter()
                .skip(p_count)
                .take(e_count)
                .copied()
                .collect();
            let e_avg_usage = if e_usage.is_empty() {
                0.0
            } else {
                e_usage.iter().sum::<f32>() / e_usage.len() as f32
            };
            let p_avg_usage = if p_usage.is_empty() {
                0.0
            } else {
                p_usage.iter().sum::<f32>() / p_usage.len() as f32
            };
            let total_cores = e_count + p_count;
            let cpu_avg_usage = if total_cores == 0 || cpu_usage.is_empty() {
                0.0
            } else {
                cpu_usage.iter().take(total_cores).sum::<f32>() / total_cores as f32
            };

            rows.push(TreeRow::pw_full(
                "cpu",
                Some("soc"),
                &format!("{}├─ ", cp),
                &format!("CPU ({} cores, {:.0}%)", e_count + p_count, cpu_avg_usage),
                s.cpu.get(),
                w.cpu,
                "",
                &temp_info("CPU"),
                Style::default(),
                pin("cpu"),
            ));
            let e_avg_temp = if e_temps.is_empty() {
                String::new()
            } else {
                let avg = e_temps.iter().sum::<f32>() / e_temps.len() as f32;
                let min = e_temps.iter().copied().fold(f32::INFINITY, f32::min);
                let max = e_temps.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                format!("{:.0}°C ({:.0}–{:.0})", avg, min, max)
            };
            let p_avg_temp = if p_temps.is_empty() {
                String::new()
            } else {
                let avg = p_temps.iter().sum::<f32>() / p_temps.len() as f32;
                let min = p_temps.iter().copied().fold(f32::INFINITY, f32::min);
                let max = p_temps.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                format!("{:.0}°C ({:.0}–{:.0})", avg, min, max)
            };

            rows.push(TreeRow::pw_full(
                "ecpu",
                Some("cpu"),
                &format!("{}│  ├─ ", cp),
                &format!("E-Cores ({} cores, {:.0}%)", e_count, e_avg_usage),
                s.ecpu.get(),
                w.ecpu,
                &fmt_freq(s.ecpu_freq.get()),
                &e_avg_temp,
                Style::default(),
                pin("ecpu"),
            ));

            // Per E-core rows (collapsed by default)
            {
                let ecpu_cont = format!("{}│  │  ", cp);
                let all_ecores: Vec<_> = m
                    .soc
                    .ecpu_clusters
                    .iter()
                    .flat_map(|cl| cl.cores.iter())
                    .collect();

                for (ci, core) in all_ecores.iter().enumerate() {
                    let pfx = if ci == all_ecores.len() - 1 {
                        format!("{}└─ ", ecpu_cont)
                    } else {
                        format!("{}├─ ", ecpu_cont)
                    };
                    let key = proc_key(&mut self.proc_keys, -(ci as i32 + 1000));
                    let temp = e_temps
                        .get(ci)
                        .map(|t| format!("{:.0}°C", t))
                        .unwrap_or_default();
                    let usage = e_usage
                        .get(ci)
                        .map(|u| format!(" ({:>3.0}%) {}", u, usage_bar(*u)))
                        .unwrap_or_default();
                    rows.push(TreeRow::pw_full(
                        key,
                        Some("ecpu"),
                        &pfx,
                        &format!("{:<10}{}", core.name, usage),
                        core.watts,
                        0.0,
                        "",
                        &temp,
                        Style::default(),
                        pin(key),
                    ));
                }
            }
            rows.push(TreeRow::pw_full(
                "pcpu",
                Some("cpu"),
                &format!("{}│  └─ ", cp),
                &format!("P-Cores ({} cores, {:.0}%)", p_count, p_avg_usage),
                s.pcpu.get(),
                w.pcpu,
                &fmt_freq(s.pcpu_freq.get()),
                &p_avg_temp,
                Style::default(),
                pin("pcpu"),
            ));
            // Per P-core rows (collapsed by default)
            {
                let pcpu_cont = format!("{}│     ", cp);
                for (ci, core) in m.soc.pcpu_cluster.cores.iter().enumerate() {
                    let pfx = if ci == m.soc.pcpu_cluster.cores.len() - 1 {
                        format!("{}└─ ", pcpu_cont)
                    } else {
                        format!("{}├─ ", pcpu_cont)
                    };
                    let key = proc_key(&mut self.proc_keys, -(ci as i32 + 2000));
                    let temp = p_temps
                        .get(ci)
                        .map(|t| format!("{:.0}°C", t))
                        .unwrap_or_default();
                    let usage = p_usage
                        .get(ci)
                        .map(|u| format!(" ({:>3.0}%) {}", u, usage_bar(*u)))
                        .unwrap_or_default();
                    rows.push(TreeRow::pw_full(
                        key,
                        Some("pcpu"),
                        &pfx,
                        &format!("{:<10}{}", core.name, usage),
                        core.watts,
                        0.0,
                        "",
                        &temp,
                        Style::default(),
                        pin(key),
                    ));
                }
            }
            let gpu_util = m.soc.gpu_util_device;
            let gpu_label = if m.gpu_cores > 0 {
                format!("GPU ({} cores, {}%)", m.gpu_cores, gpu_util)
            } else {
                format!("GPU ({}%)", gpu_util)
            };
            rows.push(TreeRow::pw_full(
                "gpu",
                Some("soc"),
                &format!("{}├─ ", cp),
                &gpu_label,
                s.gpu.get(),
                w.gpu,
                &fmt_freq(s.gpu_freq.get()),
                &temp_info("GPU"),
                Style::default(),
                pin("gpu"),
            ));
            {
                let gpu_cont = format!("{}│  ", cp);
                let gpu_color = power_color(s.gpu.get().abs());
                let cores_label = if m.gpu_cores > 0 {
                    format!(
                        "{} Cores ({:>3}%) {}",
                        m.gpu_cores,
                        gpu_util,
                        usage_bar(gpu_util as f32)
                    )
                } else {
                    format!("Cores    ({:>3}%) {}", gpu_util, usage_bar(gpu_util as f32))
                };
                rows.push(TreeRow::info(
                    Some("gpu"),
                    &format!("{}├─ ", gpu_cont),
                    &cores_label,
                    "",
                    "",
                    Style::default().fg(gpu_color),
                ));
                rows.push(TreeRow::info(
                    Some("gpu"),
                    &format!("{}├─ ", gpu_cont),
                    &format!(
                        "Renderer ({:>3}%) {}",
                        m.soc.gpu_util_renderer,
                        usage_bar(m.soc.gpu_util_renderer as f32)
                    ),
                    "",
                    "",
                    Style::default().fg(gpu_color),
                ));
                rows.push(TreeRow::info(
                    Some("gpu"),
                    &format!("{}└─ ", gpu_cont),
                    &format!(
                        "Tiler    ({:>3}%) {}",
                        m.soc.gpu_util_tiler,
                        usage_bar(m.soc.gpu_util_tiler as f32)
                    ),
                    "",
                    "",
                    Style::default().fg(gpu_color),
                ));
            }
            rows.push(TreeRow::pw_full(
                "ane",
                Some("soc"),
                &format!("{}├─ ", cp),
                "ANE",
                s.ane.get(),
                w.ane,
                "",
                &temp_info("ANE"),
                Style::default(),
                pin("ane"),
            ));
            // ANE sub-engines (collapsed by default)
            if m.soc.ane_parts.len() > 1 {
                let ane_cont = format!("{}│  ", cp);
                for (ai, (name, watts)) in m.soc.ane_parts.iter().enumerate() {
                    let pfx = if ai == m.soc.ane_parts.len() - 1 {
                        format!("{}└─ ", ane_cont)
                    } else {
                        format!("{}├─ ", ane_cont)
                    };
                    rows.push(TreeRow::pw(
                        proc_key(&mut self.proc_keys, -(ai as i32 + 3000)),
                        Some("ane"),
                        &pfx,
                        name,
                        *watts,
                        0.0,
                        Style::default(),
                        false,
                    ));
                }
            }
            let dram_name = if m.dram_gb > 0 {
                format!("DRAM ({:.1}/{} GB)", m.mem_used_gb, m.dram_gb)
            } else {
                "DRAM".into()
            };
            rows.push(TreeRow::pw_full(
                "dram",
                Some("soc"),
                &format!("{}├─ ", cp),
                &dram_name,
                s.dram.get(),
                w.dram,
                "",
                &temp_info("Memory"),
                Style::default(),
                pin("dram"),
            ));
            rows.push(TreeRow::pw(
                "gpu_sram",
                Some("soc"),
                &format!("{}├─ ", cp),
                "GPU SRAM (SLC)",
                s.gpu_sram.get(),
                w.gpu_sram,
                Style::default(),
                pin("gpu_sram"),
            ));
            rows.push(TreeRow::pw(
                "media",
                Some("soc"),
                &format!("{}├─ ", cp),
                "Media Engine",
                s.media.get(),
                w.media,
                Style::default(),
                pin("media"),
            ));
            rows.push(TreeRow::pw(
                "isp",
                Some("soc"),
                &format!("{}├─ ", cp),
                "Camera (ISP)",
                s.isp.get(),
                w.isp,
                Style::default(),
                pin("isp"),
            ));
            rows.push(TreeRow::pw(
                "fabric",
                Some("soc"),
                &format!("{}└─ ", cp),
                "Fabric",
                s.fabric.get(),
                w.fabric,
                Style::default(),
                pin("fabric"),
            ));
        }

        // ── SSD with Controller/NAND sub-items
        // Ts{N}P keys = NAND controllers, other Ts* = NAND flash dies
        let ssd_temps: Vec<&TempSensor> = m
            .temperatures
            .iter()
            .filter(|t| t.category == "SSD")
            .collect();
        let ctrl_temps: Vec<f32> = ssd_temps
            .iter()
            .filter(|t| t.key.ends_with('P'))
            .map(|t| t.value_celsius)
            .collect();
        let nand_temps: Vec<f32> = ssd_temps
            .iter()
            .filter(|t| !t.key.ends_with('P'))
            .map(|t| t.value_celsius)
            .collect();
        let fmt_temps = |v: &[f32]| -> String {
            if v.is_empty() {
                return String::new();
            }
            let avg = v.iter().sum::<f32>() / v.len() as f32;
            let mn = v.iter().copied().fold(f32::INFINITY, f32::min);
            let mx = v.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            format!("{:.0}°C ({:.0}–{:.0})", avg, mn, mx)
        };
        let ssd_temp_str = if !ctrl_temps.is_empty() {
            fmt_temps(&ctrl_temps)
        } else {
            temp_info("SSD")
        };
        rows.push(TreeRow::pw_full_est(
            "ssd",
            Some("system"),
            &t("ssd"),
            &if m.ssd_model.is_empty() {
                "SSD".into()
            } else {
                format!("SSD ({})", m.ssd_model)
            },
            s.ssd.get(),
            w.ssd,
            "",
            &ssd_temp_str,
            BOLD,
            pin("ssd"),
        ));
        {
            let sc = c("ssd");
            if !ctrl_temps.is_empty() {
                let mut r = TreeRow::info(
                    Some("ssd"),
                    &format!("{}├─ ", sc),
                    "Controller",
                    "",
                    "",
                    Style::default().fg(Color::Green),
                );
                r.temp = fmt_temps(&ctrl_temps);
                rows.push(r);
            }
            rows.push({
                let mut r = TreeRow::info(
                    Some("ssd"),
                    &format!("{}└─ ", sc),
                    "NAND Flash",
                    "",
                    "",
                    Style::default().fg(Color::Green),
                );
                r.temp = fmt_temps(&nand_temps);
                r.key = Some("ssd_nand");
                r
            });
            let mut r = TreeRow::info(
                Some("ssd_nand"),
                &format!("{}   ├─ ", sc),
                "Read",
                &human_rate(m.disk.read_bytes_per_sec),
                &human_bytes(self.wh.disk_read_bytes),
                DATA_STYLE,
            );
            r.key = Some("disk_read");
            rows.push(r);
            let mut r = TreeRow::info(
                Some("ssd_nand"),
                &format!("{}   └─ ", sc),
                "Write",
                &human_rate(m.disk.write_bytes_per_sec),
                &human_bytes(self.wh.disk_write_bytes),
                DATA_STYLE,
            );
            r.key = Some("disk_write");
            rows.push(r);
        }

        // ── Display (backlight from SMC PDBR + IOReport DISP/DISPEXT)
        {
            let has_pdbr = m.backlight_power_w > 0.0;
            let bl_w = if has_pdbr {
                s.backlight.get()
            } else {
                s.display.get()
            };
            let disp_w = bl_w + s.display_soc.get() + s.display_ext.get();
            let disp_wh = w.backlight + w.display_soc + w.display_ext;
            let name = if m.display.available {
                if m.display.nits > 0.0 {
                    format!(
                        "Display ({:.0}% brightness, {:.0}/{:.0} nits)",
                        m.display.brightness_pct, m.display.nits, m.display.max_nits
                    )
                } else if m.display.brightness_pct > 0.0 {
                    format!("Display ({:.0}% brightness)", m.display.brightness_pct)
                } else {
                    "Display (0% brightness)".into()
                }
            } else {
                "Display (off)".into()
            };
            let style = if m.display.available { BOLD } else { DIM };
            let dc = c("display");
            let disp_temp = temp_info("Display");
            let has_ext = m.soc.display_ext_w > 0.0 || w.display_ext > 0.0;
            if has_pdbr {
                let mut r = TreeRow::pw(
                    "display",
                    Some("system"),
                    &t("display"),
                    &name,
                    disp_w,
                    disp_wh,
                    style,
                    pin("display"),
                );
                r.temp = disp_temp;
                rows.push(r);
                let bl_last = if has_ext { "├─ " } else { "└─ " };
                rows.push(TreeRow::pw(
                    "backlight",
                    Some("display"),
                    &format!("{}{}", dc, bl_last),
                    "Backlight",
                    s.backlight.get(),
                    w.backlight,
                    Style::default(),
                    pin("backlight"),
                ));
            } else {
                let mut r = TreeRow::pw_est(
                    "display",
                    Some("system"),
                    &t("display"),
                    &name,
                    disp_w,
                    disp_wh,
                    style,
                    pin("display"),
                );
                r.temp = disp_temp;
                rows.push(r);
            }
            if has_ext {
                rows.push(TreeRow::pw(
                    "display_ext",
                    Some("display"),
                    &format!("{}└─ ", dc),
                    "External Display",
                    s.display_ext.get(),
                    w.display_ext,
                    Style::default(),
                    pin("display_ext"),
                ));
            }
        }

        // ── Keyboard (always show — 0% brightness is valid, not pending)
        rows.push(TreeRow::pw_est(
            "keyboard",
            Some("system"),
            &t("keyboard"),
            &format!("Keyboard ({:.0}% brightness)", m.keyboard.brightness_pct),
            s.keyboard.get(),
            w.keyboard,
            BOLD,
            pin("keyboard"),
        ));

        // ── Trackpad (temperature only, power included in SoC)
        {
            let tp_temps: Vec<&TempSensor> = m
                .temperatures
                .iter()
                .filter(|t| t.category == "Trackpad")
                .collect();
            if !tp_temps.is_empty() {
                let all_vals: Vec<f32> = tp_temps.iter().map(|t| t.value_celsius).collect();
                let tp_temp = fmt_temps(&all_vals);
                let mut r = TreeRow::info(
                    Some("system"),
                    &t("trackpad"),
                    "Trackpad",
                    "",
                    "",
                    Style::default().fg(Color::Green),
                );
                r.temp = tp_temp;
                r.key = Some("trackpad");
                rows.push(r);
                let tc = c("trackpad");
                let module = tp_temps.iter().find(|t| t.key == "TPMP");
                let surface = tp_temps.iter().find(|t| t.key == "TPSP");
                if let Some(m_temp) = module {
                    let mut r = TreeRow::info(
                        Some("trackpad"),
                        &format!("{}├─ ", tc),
                        "Module",
                        "",
                        "",
                        Style::default().fg(Color::Green),
                    );
                    r.temp = format!("{:.0}°C", m_temp.value_celsius);
                    rows.push(r);
                }
                if let Some(s_temp) = surface {
                    let mut r = TreeRow::info(
                        Some("trackpad"),
                        &format!("{}└─ ", tc),
                        "Surface",
                        "",
                        "",
                        Style::default().fg(Color::Green),
                    );
                    r.temp = format!("{:.0}°C", s_temp.value_celsius);
                    rows.push(r);
                }
            }
        }

        // ── Audio
        let audio_status = match (m.audio.device_active, m.audio.playing, m.audio.volume_pct) {
            (false, _, _) => "off".into(),
            (_, true, Some(v)) => format!(
                "{:.0}% volume{}",
                v,
                if m.audio.muted { " muted" } else { ", playing" }
            ),
            (_, true, None) => "playing".into(),
            (_, false, _) => "idle".into(),
        };
        rows.push(TreeRow::pw_est(
            "audio",
            Some("system"),
            &t("audio"),
            &format!("Audio ({})", audio_status),
            s.audio.get(),
            w.audio,
            BOLD,
            pin("audio"),
        ));

        // ── Fans
        let fc = c("fans");
        if m.fans.is_empty() {
            rows.push(TreeRow::pw_est(
                "fans",
                Some("system"),
                &t("fans"),
                "Fans (pending…)",
                0.0,
                0.0,
                PENDING,
                pin("fans"),
            ));
        } else {
            rows.push(TreeRow::pw_est(
                "fans",
                Some("system"),
                &t("fans"),
                "Fans",
                s.fan_total.get(),
                w.fans,
                BOLD,
                pin("fans"),
            ));
            rows.extend(m.fans.iter().enumerate().map(|(i, fan)| {
                let pfx = if i == m.fans.len() - 1 {
                    format!("{}└─ ", fc)
                } else {
                    format!("{}├─ ", fc)
                };
                let fan_wh_val = self.fan_wh.get(i).copied().unwrap_or(0.0);
                TreeRow::pw_est(
                    fan_key(i),
                    Some("fans"),
                    &pfx,
                    &format!(
                        "{} ({:.0}/{:.0} RPM)",
                        fan.name, fan.actual_rpm, fan.max_rpm
                    ),
                    fan.estimated_power_w,
                    fan_wh_val,
                    Style::default(),
                    pin(fan_key(i)),
                )
            }));
        }

        // ── Peripherals
        let pc = c("peripherals");
        let has_usb_power_out = m.usb_power_out_w > 0.0;
        let usb_total_w: f32 = if has_usb_power_out {
            m.usb_power_out_w
        } else {
            m.usb_devices
                .iter()
                .map(|d| d.power_ma.unwrap_or(0) as f32 * 5.0 / 1000.0)
                .sum()
        };
        let usb_total_wh: f64 = self.usb_wh.iter().sum();
        rows.push(TreeRow::pw_est(
            "peripherals",
            Some("system"),
            &t("peripherals"),
            "Peripherals",
            s.wifi.get() + s.bluetooth.get() + s.pcie.get() + usb_total_w,
            w.wifi + w.bluetooth + w.pcie + usb_total_wh,
            BOLD,
            pin("peripherals"),
        ));

        rows.push(TreeRow::pw(
            "pcie",
            Some("peripherals"),
            &format!("{}├─ ", pc),
            "Thunderbolt/PCIe",
            s.pcie.get(),
            w.pcie,
            Style::default(),
            pin("pcie"),
        ));

        // Ethernet (collapsible, collapsed by default)
        if m.ethernet.connected {
            let eth_iface = if m.ethernet.interface_name.is_empty() {
                String::new()
            } else {
                format!("{}, ", m.ethernet.interface_name)
            };
            let eth_label = if m.ethernet.link_speed_mbps >= 1000 {
                format!(
                    "Ethernet ({}{} Gbps)",
                    eth_iface,
                    m.ethernet.link_speed_mbps / 1000,
                )
            } else if m.ethernet.link_speed_mbps > 0 {
                format!(
                    "Ethernet ({}{} Mbps)",
                    eth_iface, m.ethernet.link_speed_mbps,
                )
            } else if !eth_iface.is_empty() {
                format!("Ethernet ({})", m.ethernet.interface_name)
            } else {
                "Ethernet".into()
            };
            rows.push({
                let mut r = TreeRow::info(
                    Some("peripherals"),
                    &format!("{}├─ ", pc),
                    &eth_label,
                    "",
                    "",
                    Style::default().fg(Color::Green),
                );
                r.key = Some("ethernet");
                r
            });
            let mut r = TreeRow::info(
                Some("ethernet"),
                &format!("{}│  ├─ ", pc),
                "↓ Download",
                &human_rate(s.eth_down.get() as f64),
                &human_bytes(self.wh.eth_down_bytes),
                DATA_STYLE,
            );
            r.key = Some("eth_down");
            rows.push(r);
            let mut r = TreeRow::info(
                Some("ethernet"),
                &format!("{}│  └─ ", pc),
                "↑ Upload",
                &human_rate(s.eth_up.get() as f64),
                &human_bytes(self.wh.eth_up_bytes),
                DATA_STYLE,
            );
            r.key = Some("eth_up");
            rows.push(r);
        }

        // WiFi
        let wifi_iface = if m.wifi.interface_name.is_empty() {
            String::new()
        } else {
            format!("{}, ", m.wifi.interface_name)
        };
        let (wifi_name, wifi_style) = match (m.wifi.connected, m.wifi.phy_mode.is_empty()) {
            (true, _) => {
                let ch = if m.wifi.channel.is_empty() {
                    String::new()
                } else {
                    format!(", ch{}", m.wifi.channel)
                };
                (
                    format!(
                        "WiFi ({}{} dBm, {}{})",
                        wifi_iface, m.wifi.rssi_dbm, m.wifi.phy_mode, ch
                    ),
                    Style::default(),
                )
            }
            (false, true) => {
                if wifi_iface.is_empty() {
                    ("WiFi (scanning…)".into(), PENDING)
                } else {
                    (
                        format!("WiFi ({} scanning…)", m.wifi.interface_name),
                        PENDING,
                    )
                }
            }
            (false, false) => {
                if wifi_iface.is_empty() {
                    ("WiFi (off)".into(), Style::default())
                } else {
                    (
                        format!("WiFi ({}, off)", m.wifi.interface_name),
                        Style::default(),
                    )
                }
            }
        };
        let has_wipm = m.wifi_power_w > 0.0;
        if has_wipm {
            rows.push(TreeRow::pw(
                "wifi",
                Some("peripherals"),
                &format!("{}├─ ", pc),
                &wifi_name,
                s.wifi.get(),
                w.wifi,
                wifi_style,
                pin("wifi"),
            ));
        } else {
            rows.push(TreeRow::pw_est(
                "wifi",
                Some("peripherals"),
                &format!("{}├─ ", pc),
                &wifi_name,
                s.wifi.get(),
                w.wifi,
                wifi_style,
                pin("wifi"),
            ));
        }

        let wifi_has_traffic =
            s.wifi_down.get() > 0.0 || s.wifi_up.get() > 0.0 || self.wh.wifi_down_bytes > 0.0;
        if m.wifi.connected || wifi_has_traffic {
            let mut r = TreeRow::info(
                Some("wifi"),
                &format!("{}│  ├─ ", pc),
                "↓ Download",
                &human_rate(s.wifi_down.get() as f64),
                &human_bytes(self.wh.wifi_down_bytes),
                DATA_STYLE,
            );
            r.key = Some("wifi_down");
            rows.push(r);
            let mut r = TreeRow::info(
                Some("wifi"),
                &format!("{}│  └─ ", pc),
                "↑ Upload",
                &human_rate(s.wifi_up.get() as f64),
                &human_bytes(self.wh.wifi_up_bytes),
                DATA_STYLE,
            );
            r.key = Some("wifi_up");
            rows.push(r);
        }

        let bt_name = if !m.bluetooth_devices.is_empty() {
            format!("Bluetooth ({} devices)", m.bluetooth_devices.len())
        } else {
            "Bluetooth".into()
        };
        rows.push(TreeRow::pw_est(
            "bluetooth",
            Some("peripherals"),
            &format!("{}├─ ", pc),
            &bt_name,
            s.bluetooth.get(),
            w.bluetooth,
            Style::default(),
            pin("bluetooth"),
        ));
        rows.extend(m.bluetooth_devices.iter().enumerate().map(|(i, d)| {
            let pfx = if i == m.bluetooth_devices.len() - 1 {
                format!("{}│  └─ ", pc)
            } else {
                format!("{}│  ├─ ", pc)
            };
            let bat = d
                .batteries
                .iter()
                .map(|(l, p)| {
                    if l.is_empty() {
                        p.clone()
                    } else {
                        format!("{}: {}", l, p)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            let bat_str = if !bat.is_empty() {
                format!(" [{}]", bat)
            } else {
                String::new()
            };
            TreeRow::info(
                Some("bluetooth"),
                &pfx,
                &format!("{} {}{}", d.name, d.minor_type, bat_str),
                "",
                "",
                DIM,
            )
        }));

        if m.usb_devices.is_empty() {
            rows.push(TreeRow::info(
                Some("peripherals"),
                &format!("{}└─ ", pc),
                "USB (no devices)",
                "",
                "",
                DIM,
            ));
        } else {
            let usb_row_fn = if has_usb_power_out {
                TreeRow::pw
            } else {
                TreeRow::pw_est
            };
            rows.push(usb_row_fn(
                "usb",
                Some("peripherals"),
                &format!("{}└─ ", pc),
                &format!("USB ({} devices)", m.usb_devices.len()),
                usb_total_w,
                usb_total_wh,
                Style::default(),
                pin("usb"),
            ));
            for (i, d) in m.usb_devices.iter().enumerate() {
                let pfx = if i == m.usb_devices.len() - 1 {
                    format!("{}   └─ ", pc)
                } else {
                    format!("{}   ├─ ", pc)
                };
                let cont = if i == m.usb_devices.len() - 1 {
                    format!("{}      ", pc)
                } else {
                    format!("{}   │  ", pc)
                };
                let port_idx = (d.location_id >> 20) & 0xF;
                let real_power = m
                    .usb_power_per_port
                    .iter()
                    .find(|(idx, _)| *idx == port_idx)
                    .map(|(_, w)| *w);
                let (watts, is_measured) = if let Some(rp) = real_power {
                    if rp > 0.0 {
                        (rp, true)
                    } else {
                        (d.power_ma.unwrap_or(0) as f32 * 5.0 / 1000.0, false)
                    }
                } else {
                    (d.power_ma.unwrap_or(0) as f32 * 5.0 / 1000.0, false)
                };
                let usb_wh_val = self.usb_wh.get(i).copied().unwrap_or(0.0);
                let key = usb_key(i);
                let speed_str = match d.speed {
                    0 => "1.5Mbps",
                    1 => "12Mbps",
                    2 => "480Mbps",
                    3 => "5Gbps",
                    4 => "10Gbps",
                    5 => "20Gbps",
                    _ => "?",
                };
                let pwr_str = d.power_ma.map(|p| format!(", {}mA", p)).unwrap_or_default();
                let row_fn = if is_measured {
                    TreeRow::pw
                } else {
                    TreeRow::pw_est
                };
                rows.push(row_fn(
                    key,
                    Some("usb"),
                    &pfx,
                    &format!("{} ({}{})", d.name.trim(), speed_str, pwr_str),
                    watts,
                    usb_wh_val,
                    Style::default(),
                    pin(key),
                ));
                // Data counters (children of usb device, collapsed by default)
                if d.bytes_read > 0 || d.bytes_written > 0 {
                    let (rate_r, rate_w) = self.usb_rates.get(i).copied().unwrap_or((0.0, 0.0));
                    rows.push(TreeRow::info(
                        Some(key),
                        &format!("{}├─ ", cont),
                        "Read",
                        &human_rate(rate_r),
                        &human_bytes(d.bytes_read as f64),
                        DATA_STYLE,
                    ));
                    rows.push(TreeRow::info(
                        Some(key),
                        &format!("{}└─ ", cont),
                        "Write",
                        &human_rate(rate_w),
                        &human_bytes(d.bytes_written as f64),
                        DATA_STYLE,
                    ));
                }
            }
        }

        // ── Software (standalone collapsible section after the tree)
        rows.push(TreeRow::separator());
        let all_sw_energy =
            (m.all_procs_energy_mj - self.proc_baseline.values().sum::<f64>()).max(0.0);
        // Dynamic limit: count only visible rows (after collapse filtering)
        let visible_tree_rows = rows.iter().filter(|r| !self.is_hidden(r, &rows)).count();
        let chart_slots = if self.pinned.is_empty() {
            1
        } else {
            self.pinned.len() + 1
        };
        let reserved = visible_tree_rows + 5 + chart_slots * CHART_HEIGHT as usize;
        let proc_limit = ((self.term_height as usize).saturating_sub(reserved)).max(10);
        {
            let mut sw_row = TreeRow::pw(
                "software",
                None,
                "",
                &format!("Software (filter: top {} by total)", proc_limit),
                m.all_procs_power_w + 0.0,
                all_sw_energy / 3600.0 / 1000.0,
                BOLD,
                pin("software"),
            );
            sw_row.current = format!("{:>5.1} mW", m.all_procs_power_w * 1000.0);
            sw_row.label_style = BOLD;
            rows.push(sw_row);
        }
        if m.top_processes.is_empty() {
            rows.push(TreeRow::info(
                Some("software"),
                "",
                "(collecting…)",
                "",
                "",
                PENDING,
            ));
        }
        let self_pid = std::process::id() as i32;
        let baseline = &self.proc_baseline;
        let mut display_procs: Vec<_> = m
            .top_processes
            .iter()
            .map(|p| {
                let base = baseline.get(&p.pid).copied().unwrap_or(0.0);
                (p, (p.energy_mj - base).max(0.0))
            })
            .filter(|(_, adj)| *adj > 0.0)
            .collect();
        display_procs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        display_procs.truncate(proc_limit);
        // Pre-compute per-process keys
        let proc_row_keys: Vec<&'static str> = display_procs
            .iter()
            .map(|(p, _)| proc_key(&mut self.proc_keys, p.pid))
            .collect();
        rows.extend(display_procs.iter().enumerate().map(|(i, (p, adj_mj))| {
            let pfx = if i == display_procs.len() - 1 {
                "└─ "
            } else {
                "├─ "
            };
            let dead = !p.alive;
            let color = if dead {
                Color::DarkGray
            } else if p.pid == self_pid {
                Color::Blue
            } else {
                power_color(p.power_w)
            };
            let key = proc_row_keys[i];
            let label = if dead {
                format!("{} ({}) [dead]", p.name, p.pid)
            } else {
                format!("{} ({})", p.name, p.pid)
            };
            let mut r = TreeRow::pw(
                key,
                Some("software"),
                pfx,
                &label,
                p.power_w,
                *adj_mj / 3600.0 / 1000.0,
                Style::default().fg(color),
                self.pinned.contains(&key),
            );
            r.current = format!("{:>5.1} mW", p.power_w * 1000.0);
            r.current_style = Style::default().fg(color);
            r
        }));

        rows
    }

    // ── Two-pass buffer renderer ────────────────────────────────────────────

    fn draw_tree_buf(
        &mut self,
        f: &mut Frame,
        area: Rect,
        rows: &[&TreeRow],
        all_rows: &[TreeRow],
    ) {
        let block = Block::default().borders(Borders::ALL).title(format!(
            " Power Tree ({}/{}) ",
            self.cursor + 1,
            rows.len()
        ));
        let inner = block.inner(area);
        f.render_widget(block, area);

        if inner.width < 20 || inner.height < 3 {
            return;
        }
        let buf = f.buffer_mut();

        let hdr_y = inner.y;
        let right = inner.right();
        // Inline sparkline column when wide enough (1-char gap after Total)
        let spark_gap: u16 = if inner.width > 90 { 1 } else { 0 };
        let spark_w = if inner.width > 90 {
            (inner.width - 90 - 1).min(60)
        } else {
            0
        };
        let tot_x = right
            .saturating_sub(COL_TOT)
            .saturating_sub(spark_w)
            .saturating_sub(spark_gap);
        let cur_x = tot_x.saturating_sub(COL_CUR);
        let tmp_x = cur_x.saturating_sub(COL_TEMP);
        let frq_x = tmp_x.saturating_sub(COL_FREQ);
        let spark_x = right.saturating_sub(spark_w);

        buf.set_string(inner.x + 2, hdr_y, "Component", BOLD);
        right_str(buf, frq_x, hdr_y, COL_FREQ, "Freq", BOLD);
        right_str(buf, tmp_x, hdr_y, COL_TEMP, "Temp", BOLD);
        right_str(buf, cur_x, hdr_y, COL_CUR, "Power", BOLD);
        right_str(buf, tot_x, hdr_y, COL_TOT, "Total", BOLD);
        if spark_w > 0 {
            right_str(buf, spark_x, hdr_y, spark_w, "History", BOLD);
        }

        let data_y = hdr_y + 1;
        let vis_h = inner.height.saturating_sub(1) as usize;
        let total = rows.len();
        let scroll = self.scroll_offset(vis_h, total);
        self.tree_data_y = data_y;
        self.tree_scroll = scroll;
        self.tree_vis_h = vis_h;
        let pin_w: u16 = 2;
        let tree_x = inner.x + pin_w;

        for (vi, row) in rows.iter().skip(scroll).take(vis_h).enumerate() {
            let y = data_y + vi as u16;
            let abs_idx = scroll + vi;

            // Full-width separator line
            if row.label == "\x00sep" {
                let line = "─".repeat(inner.width as usize);
                buf.set_string(inner.x, y, &line, TREE_STYLE);
                continue;
            }

            // Pin gutter (fixed 2-char column at the left)
            if row.pinned {
                buf.set_string(inner.x, y, PIN_MARKER, Style::default().fg(Color::Cyan));
            }

            // Draw prefix (tree chars in white)
            buf.set_string(tree_x, y, &row.prefix, TREE_STYLE);

            // Draw label after prefix — extends freely up to Current column
            let label_x = tree_x + row.prefix.width() as u16;
            let is_parent = row.has_children_in(all_rows);
            let is_collapsed = row.key.map(|k| self.collapsed.contains(k)).unwrap_or(false);
            let indicator = if is_parent {
                if is_collapsed {
                    "▸ "
                } else {
                    "▾ "
                }
            } else {
                ""
            };
            let max_label_w = cur_x.saturating_sub(label_x) as usize;
            if is_parent {
                buf.set_string(label_x, y, indicator, TREE_STYLE);
                let lbl_start = label_x + indicator.width() as u16;
                let lbl_text =
                    truncate_str(&row.label, max_label_w.saturating_sub(indicator.width()));
                buf.set_string(lbl_start, y, &lbl_text, row.label_style);
            } else {
                let full_label = format!("{}{}", indicator, row.label);
                let truncated_label = truncate_str(&full_label, max_label_w);
                buf.set_string(label_x, y, &truncated_label, row.label_style);
            }

            // Overlay data columns (Freq/Temp overwrite label text where needed)
            if !row.freq.is_empty() {
                // Clear column and write with left padding
                buf.set_string(frq_x, y, " ".repeat(COL_FREQ as usize), Style::default());
                right_str(buf, frq_x, y, COL_FREQ, &row.freq, DIM);
            }
            if !row.temp.is_empty() {
                buf.set_string(tmp_x, y, " ".repeat(COL_TEMP as usize), Style::default());
                right_str(buf, tmp_x, y, COL_TEMP, &row.temp, DIM);
            }
            if !row.current.is_empty() {
                right_str(buf, cur_x, y, COL_CUR, &row.current, row.current_style);
            }
            if !row.total.is_empty() {
                right_str(buf, tot_x, y, COL_TOT, &row.total, DIM);
            }

            // Inline sparkline column (1-char height, block chars)
            if spark_w > 0 {
                if let Some(key) = row.key {
                    if let Some(hist) = self.history.get(key) {
                        let w = spark_w as usize;
                        let skip = hist.len().saturating_sub(w);
                        let visible: Vec<f64> = hist.iter().skip(skip).copied().collect();
                        let vis_max = visible.iter().copied().fold(0.0f64, f64::max).max(0.001);
                        let is_data_key = matches!(
                            key,
                            "eth_down"
                                | "eth_up"
                                | "wifi_down"
                                | "wifi_up"
                                | "disk_read"
                                | "disk_write"
                        );
                        for (ci, &val) in visible.iter().enumerate() {
                            let x = spark_x + (w - visible.len() + ci) as u16;
                            let level = (val / vis_max * 7.0).round() as usize;
                            let ch = SPARK_CHARS[level.min(7)];
                            let color = if is_data_key {
                                Color::Rgb(80, 140, 255)
                            } else {
                                power_color(val as f32)
                            };
                            buf.set_string(x, y, ch.to_string(), Style::default().fg(color));
                        }
                    }
                }
            }

            // Cursor highlight (background only, preserves text and fg colors)
            if abs_idx == self.cursor {
                for cx in inner.x..inner.right() {
                    if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(cx, y)) {
                        cell.set_bg(Color::Rgb(50, 50, 60));
                    }
                }
            }
        }
    }

    fn scroll_offset(&self, vis_h: usize, total: usize) -> usize {
        if self.cursor < vis_h / 3 {
            0
        } else if self.cursor > total.saturating_sub(vis_h * 2 / 3) {
            total.saturating_sub(vis_h)
        } else {
            self.cursor.saturating_sub(vis_h / 3)
        }
    }

    // ── Header / Footer / Charts ────────────────────────────────────────────

    #[allow(dead_code)]
    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" macpow — Apple Silicon Power Monitor ");
        f.render_widget(block, area);
    }

    fn draw_charts(&self, f: &mut Frame, area: Rect, keys: &[&'static str]) {
        if keys.is_empty() || area.height == 0 {
            return;
        }

        let constraints: Vec<Constraint> = keys
            .iter()
            .map(|_| Constraint::Length(CHART_HEIGHT))
            .collect();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        for (i, &key) in keys.iter().enumerate() {
            if i >= chunks.len() {
                break;
            }
            let data = self.history.get(key);
            let current = data.and_then(|b| b.back().copied()).unwrap_or(0.0);

            let is_pinned = self.pinned.contains(&key);
            let title_style = if is_pinned {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::Reset)
            };
            let pin_icon = if is_pinned { " [pinned]" } else { "" };

            let chart_area = chunks[i];
            let inner = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(6), Constraint::Min(0)])
                .split(chart_area);

            // Visible width = sparkline area minus borders
            let vis_w = inner[1].width.saturating_sub(2) as usize;

            // Take only the visible tail of the data and scale to its max
            let visible_data: Vec<f64> = data
                .map(|b| {
                    let skip = b.len().saturating_sub(vis_w);
                    b.iter().skip(skip).copied().collect()
                })
                .unwrap_or_default();
            let vis_max = visible_data.iter().copied().fold(0.0f64, f64::max);
            let scale_max = nice_scale(vis_max);

            let is_data = matches!(
                key,
                "eth_down" | "eth_up" | "wifi_down" | "wifi_up" | "disk_read" | "disk_write"
            );
            let scale_h = inner[0].height;
            let fmt_axis = |v: f64| -> String {
                if is_data {
                    human_rate(v)
                } else if v.abs() < 0.1 && v.abs() > 0.0 {
                    format!("{:>3.0}mW", v * 1000.0)
                } else {
                    format!("{:>5.1}", v)
                }
            };
            let scale_lines: Vec<Line> = (0..scale_h)
                .map(|row| {
                    if row == 0 {
                        Line::from(Span::styled(fmt_axis(scale_max), DIM))
                    } else if row == scale_h / 2 {
                        Line::from(Span::styled(fmt_axis(scale_max / 2.0), DIM))
                    } else if row == scale_h - 1 {
                        Line::from(Span::styled(fmt_axis(0.0), DIM))
                    } else {
                        Line::from("")
                    }
                })
                .collect();
            f.render_widget(Paragraph::new(scale_lines), inner[0]);

            let title_value = if is_data {
                human_rate(current)
            } else if current.abs() < 0.1 && current.abs() > 0.0 {
                format!("{:.1} mW", current * 1000.0)
            } else {
                format!("{:.3} W", current)
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(DIM)
                .title(Span::styled(
                    format!(
                        " {} — {}{}",
                        self.labels.get(key).map(|s| s.as_str()).unwrap_or(key),
                        title_value,
                        pin_icon
                    ),
                    title_style,
                ));
            let chart_inner = block.inner(inner[1]);
            f.render_widget(block, inner[1]);

            if chart_inner.width == 0 || chart_inner.height == 0 || scale_max <= 0.0 {
                continue;
            }

            let buf = f.buffer_mut();
            let inner_h = chart_inner.height as usize;
            let max_eighths = inner_h * 8;
            let bottom_y = chart_inner.y + chart_inner.height - 1;

            for (ci, &val) in visible_data.iter().enumerate() {
                let x = chart_inner.x + (vis_w.saturating_sub(visible_data.len()) + ci) as u16;
                if x >= chart_inner.right() {
                    continue;
                }
                let color = if is_data {
                    Color::Rgb(80, 140, 255)
                } else {
                    power_color(val as f32)
                };
                let bar_eighths =
                    ((val / scale_max * max_eighths as f64).round() as usize).min(max_eighths);
                let full_rows = bar_eighths / 8;
                let remainder = bar_eighths % 8;
                let style = Style::default().fg(color);

                for row in 0..full_rows {
                    let y = bottom_y.saturating_sub(row as u16);
                    if y >= chart_inner.y {
                        buf.set_string(x, y, "█", style);
                    }
                }
                if remainder > 0 {
                    let y = bottom_y.saturating_sub(full_rows as u16);
                    if y >= chart_inner.y {
                        buf.set_string(x, y, BAR_EIGHTHS[remainder].to_string(), style);
                    }
                }
            }
        }
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let footer = Paragraph::new(Line::from(vec![
            Span::styled(" q", Style::default().fg(Color::Yellow)),
            Span::raw(" quit  "),
            Span::styled("r", Style::default().fg(Color::Yellow)),
            Span::raw(" reset  "),
            Span::styled("a", Style::default().fg(Color::Yellow)),
            Span::raw(format!(" avg:{}s  ", self.sma_window)),
            Span::styled("l", Style::default().fg(Color::Yellow)),
            Span::raw(format!(" {}ms  ", self.latency_ms)),
            Span::styled("↑/↓", Style::default().fg(Color::Yellow)),
            Span::raw(" select  "),
            Span::styled("←/→", Style::default().fg(Color::Yellow)),
            Span::raw(" fold  "),
            Span::styled("+/-", Style::default().fg(Color::Yellow)),
            Span::raw(" all  "),
            Span::styled("space", Style::default().fg(Color::Yellow)),
            Span::raw(" pin    "),
            Span::styled("■", Style::default().fg(Color::Rgb(46, 139, 87))),
            Span::raw("<1W "),
            Span::styled("■", Style::default().fg(Color::Rgb(220, 180, 0))),
            Span::raw("<5W "),
            Span::styled("■", Style::default().fg(Color::Rgb(255, 140, 0))),
            Span::raw("<10W "),
            Span::styled("■", Style::default().fg(Color::Rgb(255, 50, 50))),
            Span::raw("≥10W "),
            Span::styled("■", PENDING),
            Span::raw("pending"),
        ]));
        f.render_widget(footer, area);
    }
}

// ── Buffer helpers ──────────────────────────────────────────────────────────

fn right_str(buf: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    let tw = text.width() as u16;
    let start = if tw >= width { x } else { x + width - tw };
    buf.set_string(start, y, text, style);
}

fn truncate_str(s: &str, max_w: usize) -> String {
    if s.width() <= max_w {
        return s.to_string();
    }
    let mut w = 0;
    let mut end = 0;
    for (i, ch) in s.char_indices() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > max_w.saturating_sub(1) {
            break;
        }
        w += cw;
        end = i + ch.len_utf8();
    }
    format!("{}…", &s[..end])
}

// ── Pure helpers ────────────────────────────────────────────────────────────

fn temps_by_category(temps: &[TempSensor]) -> BTreeMap<String, Vec<f32>> {
    temps
        .iter()
        .filter(|t| t.category != "Other")
        .fold(BTreeMap::new(), |mut m, t| {
            m.entry(t.category.clone())
                .or_default()
                .push(t.value_celsius);
            m
        })
}

fn selected_cpu_core_temps(
    temps: &[TempSensor],
    e_count: usize,
    p_count: usize,
) -> (Vec<f32>, Vec<f32>) {
    let mut e_temps = Vec::new();
    let mut p_temps = Vec::new();

    let e_candidates = sorted_temp_values(temps, |key| key.starts_with("Tex"));
    if e_count > 0 && e_candidates.len() == e_count {
        e_temps = e_candidates;
    }

    let p_triplets = sorted_temp_values(temps, |key| {
        key.starts_with("Tp1") || key.starts_with("Tp2")
    });
    if p_count > 0 && p_triplets.len() == p_count * 3 {
        p_temps = p_triplets
            .chunks_exact(3)
            .map(|chunk| chunk.iter().copied().fold(f32::NEG_INFINITY, f32::max))
            .collect();
    }

    if e_temps.len() == e_count && p_temps.len() == p_count {
        return (e_temps, p_temps);
    }

    let fallback = sorted_temp_values(temps, |key| key.starts_with("Tp0"));
    if fallback.len() >= e_count + p_count
        && !is_placeholder_temp_bank(&fallback, e_count + p_count)
    {
        if e_temps.len() != e_count {
            e_temps = fallback.iter().take(e_count).copied().collect();
        }
        if p_temps.len() != p_count {
            p_temps = fallback
                .iter()
                .skip(e_count)
                .take(p_count)
                .copied()
                .collect();
        }
    }

    (e_temps, p_temps)
}

fn sorted_temp_values<F>(temps: &[TempSensor], predicate: F) -> Vec<f32>
where
    F: Fn(&str) -> bool,
{
    let mut vals: Vec<_> = temps
        .iter()
        .filter(|t| t.category == "CPU" && predicate(&t.key))
        .map(|t| (t.key.as_str(), t.value_celsius))
        .collect();
    vals.sort_by(|a, b| a.0.cmp(b.0));
    vals.into_iter().map(|(_, temp)| temp).collect()
}

fn is_placeholder_temp_bank(vals: &[f32], expected_count: usize) -> bool {
    if vals.len() <= expected_count.saturating_mul(2) {
        return false;
    }
    let min = vals.iter().copied().fold(f32::INFINITY, f32::min);
    let max = vals.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    max - min < 0.25
}

fn stats(vals: &[f32]) -> (f32, f32, f32) {
    let sum: f32 = vals.iter().sum();
    let min = vals.iter().copied().fold(f32::INFINITY, f32::min);
    let max = vals.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    (sum / vals.len() as f32, min, max)
}

fn nice_scale(max_val: f64) -> f64 {
    if max_val <= 0.0 {
        return 1.0;
    }
    let steps = [
        0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0,
        100.0, 200.0,
    ];
    steps
        .iter()
        .copied()
        .find(|&s| s >= max_val)
        .unwrap_or(max_val.ceil().max(1.0))
}

fn usage_bar(pct: f32) -> String {
    let width = 10;
    let filled = ((pct / 100.0) * width as f32).round() as usize;
    let empty = width - filled.min(width);
    format!("{}{}", "▓".repeat(filled), "░".repeat(empty))
}

fn fan_key(index: usize) -> &'static str {
    const KEYS: [&str; 8] = [
        "fan0", "fan1", "fan2", "fan3", "fan4", "fan5", "fan6", "fan7",
    ];
    KEYS.get(index).copied().unwrap_or("fan0")
}

fn usb_key(index: usize) -> &'static str {
    const KEYS: [&str; 16] = [
        "usb0", "usb1", "usb2", "usb3", "usb4", "usb5", "usb6", "usb7", "usb8", "usb9", "usb10",
        "usb11", "usb12", "usb13", "usb14", "usb15",
    ];
    KEYS.get(index).copied().unwrap_or("usb0")
}

// Intentional Box::leak: history/charts require &'static str keys. Dead PIDs stay
// in history so their sparklines remain visible. Bounded by unique PIDs per session
// (~hundreds, ~20 bytes each). Cleared implicitly when the process exits.
fn proc_key(cache: &mut std::collections::HashMap<i32, &'static str>, pid: i32) -> &'static str {
    cache
        .entry(pid)
        .or_insert_with(|| Box::leak(format!("pid.{}", pid).into_boxed_str()))
}

fn read_machine_name() -> String {
    let chip = std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    let model = std::process::Command::new("sysctl")
        .args(["-n", "hw.model"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    match (chip, model) {
        (Some(c), Some(m)) => format!("{} ({})", c, m),
        (Some(c), None) => c,
        (None, Some(m)) => m,
        _ => "Mac".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu_sensor(key: &str, temp: f32) -> TempSensor {
        TempSensor {
            key: key.to_string(),
            category: "CPU".to_string(),
            value_celsius: temp,
        }
    }

    #[test]
    fn selects_modern_cpu_core_temps() {
        let mut temps = vec![
            cpu_sensor("Tex2", 42.0),
            cpu_sensor("Tex0", 40.0),
            cpu_sensor("Tex3", 43.0),
            cpu_sensor("Tex1", 41.0),
        ];

        for (idx, base) in [50.0, 55.0, 60.0, 65.0].into_iter().enumerate() {
            temps.push(cpu_sensor(&format!("Tp1{}a", idx), base - 10.0));
            temps.push(cpu_sensor(&format!("Tp1{}b", idx), base - 5.0));
            temps.push(cpu_sensor(&format!("Tp1{}c", idx), base));
        }
        for (idx, base) in [70.0, 71.0, 72.0, 73.0, 74.0, 75.0].into_iter().enumerate() {
            temps.push(cpu_sensor(&format!("Tp2{}a", idx), base - 10.0));
            temps.push(cpu_sensor(&format!("Tp2{}b", idx), base - 5.0));
            temps.push(cpu_sensor(&format!("Tp2{}c", idx), base));
        }

        let (e_temps, p_temps) = selected_cpu_core_temps(&temps, 4, 10);
        assert_eq!(e_temps, vec![40.0, 41.0, 42.0, 43.0]);
        assert_eq!(
            p_temps,
            vec![50.0, 55.0, 60.0, 65.0, 70.0, 71.0, 72.0, 73.0, 74.0, 75.0]
        );
    }

    #[test]
    fn falls_back_to_legacy_tp0_mapping() {
        let temps: Vec<_> = (0..14)
            .map(|idx| cpu_sensor(&format!("Tp0{idx:02}"), 35.0 + idx as f32))
            .collect();
        let (e_temps, p_temps) = selected_cpu_core_temps(&temps, 4, 10);
        assert_eq!(e_temps, vec![35.0, 36.0, 37.0, 38.0]);
        assert_eq!(
            p_temps,
            vec![39.0, 40.0, 41.0, 42.0, 43.0, 44.0, 45.0, 46.0, 47.0, 48.0]
        );
    }

    #[test]
    fn ignores_flat_placeholder_tp0_bank() {
        let temps: Vec<_> = (0..40)
            .map(|idx| cpu_sensor(&format!("Tp0{idx:02}"), 40.0))
            .collect();
        let (e_temps, p_temps) = selected_cpu_core_temps(&temps, 4, 10);
        assert!(e_temps.is_empty());
        assert!(p_temps.is_empty());
    }
}
