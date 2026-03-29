use macpow::sma::TimeSma;
use macpow::types::*;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Sparkline};
use ratatui::Frame;
use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

// ── Styles ───────────────────────────────────────────────────────────────────

const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const DIM: Style = Style::new().fg(Color::DarkGray);
const PENDING: Style = Style::new().fg(Color::Magenta);
const CURSOR_BG: Style = Style::new().bg(Color::Rgb(40, 40, 50));
const TREE_STYLE: Style = Style::new().fg(Color::White);
const PIN_MARKER: &str = "▸ ";
const HISTORY_LEN: usize = 240;
const CHART_HEIGHT: u16 = 7;

const COL_FREQ: u16 = 10;
const COL_TEMP: u16 = 16;
const COL_CUR: u16 = 14;
const COL_TOT: u16 = 14;

const SPARK_CHARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '▇'];

fn power_color(w: f32) -> Color {
    match w {
        w if w < 1.0 => Color::Green,
        w if w < 5.0 => Color::Yellow,
        w if w < 10.0 => Color::Rgb(255, 165, 0), // orange
        _ => Color::Rgb(255, 50, 50),              // bright red
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
        let w = watts + 0.0; // normalize -0.0 to 0.0
        Self {
            prefix: prefix.to_string(),
            label: label.to_string(),
            freq: String::new(),
            temp: String::new(),
            current: format!("{:>7.3} W", w),
            total: fmt_wh(wh),
            label_style: style.fg(style.fg.unwrap_or(power_color(w.abs()))),
            current_style: Style::default().fg(power_color(w.abs())),
            key: Some(key),
            parent,
            is_header: false,
            pinned,
        }
    }

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
        let w = watts + 0.0;
        Self {
            prefix: prefix.to_string(),
            label: label.to_string(),
            freq: freq.to_string(),
            temp: temp.to_string(),
            current: format!("{:>7.3} W", w),
            total: fmt_wh(wh),
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

    fn header(
        parent: Option<&'static str>,
        prefix: &str,
        label: &str,
        col3: &str,
        col4: &str,
    ) -> Self {
        Self {
            prefix: prefix.to_string(),
            label: label.to_string(),
            freq: String::new(),
            temp: String::new(),
            current: col3.to_string(),
            total: col4.to_string(),
            label_style: BOLD,
            current_style: BOLD,
            key: None,
            parent,
            is_header: true,
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
    ssd: f64,
    display: f64,
    keyboard: f64,
    audio: f64,
    fans: f64,
    wifi: f64,
    bluetooth: f64,
    sys: f64,
    battery: f64,
    net_down_bytes: f64,
    net_up_bytes: f64,
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
    soc_total, cpu, ecpu, pcpu, gpu, ane, dram, gpu_sram, ssd, display, keyboard, audio, fan_total,
    wifi, bluetooth, sys, battery, net_down, net_up, ecpu_freq, pcpu_freq, gpu_freq
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
        self.ssd.push(m.ssd_power_w);
        self.display.push(m.display.estimated_power_w);
        self.keyboard.push(m.keyboard.estimated_power_w);
        self.audio.push(m.audio.estimated_power_w);
        self.fan_total
            .push(m.fans.iter().map(|f| f.estimated_power_w).sum());
        self.wifi.push(m.wifi.estimated_power_w);
        self.bluetooth.push(m.bluetooth_power_w);
        self.sys.push(m.sys_power_w);
        self.battery.push(m.battery.drain_w as f32);
        self.net_down.push(m.network.bytes_in_per_sec as f32);
        self.net_up.push(m.network.bytes_out_per_sec as f32);
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
    temp_min: BTreeMap<String, f32>,
    temp_max: BTreeMap<String, f32>,
    temp_sum: BTreeMap<String, f64>,
    temp_count: BTreeMap<String, u64>,
    history: BTreeMap<&'static str, VecDeque<f64>>,
    pinned: Vec<&'static str>,
    collapsed: std::collections::HashSet<&'static str>,
    total_rows: usize,
    row_keys_cache: Vec<Option<&'static str>>,
    row_parents_cache: Vec<Option<&'static str>>,
    row_is_sep: Vec<bool>,
    labels: BTreeMap<&'static str, String>,
    proc_baseline: std::collections::HashMap<i32, f64>,
    proc_keys: std::collections::HashMap<i32, &'static str>,
    fan_wh: Vec<f64>,
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
            cursor: 0,
            last_tick: None,
            wh: Wh::default(),
            sma: MetricsSma::new(0.0),
            sma_window: 0,
            temp_min: BTreeMap::new(),
            temp_max: BTreeMap::new(),
            temp_sum: BTreeMap::new(),
            temp_count: BTreeMap::new(),
            history: BTreeMap::new(),
            pinned: Vec::new(),
            collapsed: std::collections::HashSet::new(),
            total_rows: 0,
            row_keys_cache: Vec::new(),
            row_parents_cache: Vec::new(),
            row_is_sep: Vec::new(),
            labels: BTreeMap::new(),
            proc_baseline: std::collections::HashMap::new(),
            proc_keys: std::collections::HashMap::new(),
            fan_wh: Vec::new(),
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
            self.wh.ssd += m.ssd_power_w as f64 * dt_h;
            self.wh.display += m.display.estimated_power_w as f64 * dt_h;
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
            self.wh.wifi += m.wifi.estimated_power_w as f64 * dt_h;
            self.wh.bluetooth += m.bluetooth_power_w as f64 * dt_h;
            self.wh.sys += m.sys_power_w as f64 * dt_h;
            self.wh.battery += m.battery.drain_w * dt_h;
            self.wh.net_down_bytes += m.network.bytes_in_per_sec * dt_s;
            self.wh.net_up_bytes += m.network.bytes_out_per_sec * dt_s;
        }
        self.last_tick = Some(Instant::now());

        self.push_history("soc", m.soc.total_w as f64);
        self.push_history("cpu", m.soc.cpu_w as f64);
        self.push_history("ecpu", m.soc.ecpu_total_w() as f64);
        self.push_history("pcpu", m.soc.pcpu_total_w() as f64);
        self.push_history("gpu", m.soc.gpu_w as f64);
        self.push_history("ane", m.soc.ane_w as f64);
        self.push_history("dram", m.soc.dram_w as f64);
        self.push_history("gpu_sram", m.soc.gpu_sram_w as f64);
        self.push_history("ssd", m.ssd_power_w as f64);
        self.push_history("display", m.display.estimated_power_w as f64);
        self.push_history("keyboard", m.keyboard.estimated_power_w as f64);
        self.push_history("audio", m.audio.estimated_power_w as f64);
        self.push_history(
            "fans",
            m.fans.iter().map(|f| f.estimated_power_w as f64).sum(),
        );
        for (i, fan) in m.fans.iter().enumerate() {
            self.push_history(fan_key(i), fan.estimated_power_w as f64);
        }
        self.push_history("wifi", m.wifi.estimated_power_w as f64);
        self.push_history("bluetooth", m.bluetooth_power_w as f64);
        self.push_history(
            "peripherals",
            (m.wifi.estimated_power_w + m.bluetooth_power_w) as f64,
        );
        self.push_history("system", m.sys_power_w as f64);
        self.push_history("battery", m.battery.drain_w.abs());
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
        use kbd::hotkey::{Hotkey, Modifier};
        use kbd::key::Key;
        use kbd_crossterm::CrosstermEventExt;

        // Physical key matching (works when terminal supports enhanced keyboard)
        if let Some(hotkey) = key.to_hotkey() {
            if hotkey == Hotkey::new(Key::Q) {
                return true;
            }
            if hotkey == Hotkey::new(Key::C).modifier(Modifier::Ctrl) {
                return true;
            }
            if hotkey == Hotkey::new(Key::K) {
                self.move_cursor(-1);
            }
            if hotkey == Hotkey::new(Key::J) {
                self.move_cursor(1);
            }
            if hotkey == Hotkey::new(Key::R) {
                self.reset();
            }
            if hotkey == Hotkey::new(Key::A) {
                self.cycle_sma();
            }
            if hotkey == Hotkey::new(Key::H) {
                self.collapse_or_parent();
            }
            if hotkey == Hotkey::new(Key::L) {
                self.expand_or_child();
            }
        }

        // Character fallback (for terminals without enhanced keyboard / non-Latin layouts)
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('й') => return true,
            KeyCode::Char('c') | KeyCode::Char('с')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                return true
            }
            KeyCode::Char('k') | KeyCode::Char('л') => self.move_cursor(-1),
            KeyCode::Char('j') | KeyCode::Char('о') => self.move_cursor(1),
            KeyCode::Char('r') | KeyCode::Char('к') => self.reset(),
            KeyCode::Char('a') | KeyCode::Char('ф') => self.cycle_sma(),
            KeyCode::Char('h') | KeyCode::Char('р') => self.collapse_or_parent(),
            KeyCode::Char('l') | KeyCode::Char('д') => self.expand_or_child(),
            KeyCode::Esc => return true,
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Left => self.collapse_or_parent(),
            KeyCode::Right => self.expand_or_child(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::PageUp => self.move_cursor(-10),
            KeyCode::PageDown => self.move_cursor(10),
            KeyCode::Char(' ') => self.toggle_pin(),
            _ => {}
        }
        false
    }

    fn move_cursor(&mut self, delta: i32) {
        let max = self.total_rows.saturating_sub(1) as i32;
        let mut pos = (self.cursor as i32 + delta).clamp(0, max);
        let dir = if delta >= 0 { 1 } else { -1 };
        // Skip separator rows
        while pos >= 0 && pos <= max && self.row_is_sep.get(pos as usize).copied().unwrap_or(false) {
            pos += dir;
        }
        self.cursor = pos.clamp(0, max) as usize;
    }

    pub fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            let y = mouse.row;
            if y >= self.tree_data_y {
                let vi = (y - self.tree_data_y) as usize;
                if vi < self.tree_vis_h {
                    let target = self.tree_scroll + vi;
                    if target < self.total_rows
                        && !self.row_is_sep.get(target).copied().unwrap_or(false)
                    {
                        self.cursor = target;
                    }
                }
            }
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
                if self.row_parents_cache.iter().any(|p| *p == Some(*key)) {
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

    fn reset(&mut self) {
        self.wh = Wh::default();
        self.sma.clear_all();
        self.temp_min.clear();
        self.temp_max.clear();
        self.temp_sum.clear();
        self.temp_count.clear();
        self.history.clear();
        self.fan_wh.iter_mut().for_each(|v| *v = 0.0);
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

    pub fn draw(&mut self, f: &mut Frame) {
        self.term_height = f.area().height;
        let all_rows = self.build_rows();

        // Filter out children of collapsed nodes
        let rows: Vec<&TreeRow> = all_rows
            .iter()
            .filter(|r| !self.is_hidden(r, &all_rows))
            .collect();

        self.total_rows = rows.len();
        self.cursor = self.cursor.min(self.total_rows.saturating_sub(1));

        self.row_keys_cache = rows.iter().map(|r| r.key).collect();
        self.row_parents_cache = rows.iter().map(|r| r.parent).collect();
        self.row_is_sep = rows.iter().map(|r| r.label == "\x00sep").collect();

        // Cache labels for chart titles
        for r in &rows {
            if let Some(key) = r.key {
                if !r.label.is_empty() {
                    self.labels.insert(key, r.label.clone());
                }
            }
        }

        let cursor_key = self.row_keys_cache.get(self.cursor).copied().flatten();
        let chart_keys = self.chart_keys(cursor_key);
        let chart_h = if chart_keys.is_empty() {
            0
        } else {
            chart_keys.len() as u16 * CHART_HEIGHT
        };

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
        // Collect all chart keys: cursor + pinned, ordered by tree position
        let mut entries: Vec<(usize, &'static str)> = Vec::new();
        if let Some(ck) = cursor_key {
            if !self.pinned.contains(&ck) {
                let pos = self
                    .row_keys_cache
                    .iter()
                    .position(|k| *k == Some(ck))
                    .unwrap_or(0);
                entries.push((pos, ck));
            }
        }
        for &pk in &self.pinned {
            let pos = self
                .row_keys_cache
                .iter()
                .position(|k| *k == Some(pk))
                .unwrap_or(usize::MAX);
            entries.push((pos, pk));
        }
        entries.sort_by_key(|(pos, _)| *pos);
        entries.into_iter().map(|(_, k)| k).collect()
    }

    // ── Build rows ──────────────────────────────────────────────────────────

    fn build_rows(&mut self) -> Vec<TreeRow> {
        let m = &self.metrics;
        let w = &self.wh;
        let s = &self.sma;
        let pin = |key: &str| -> bool { self.pinned.contains(&key) };

        let temp_groups = temps_by_category(&m.temperatures);
        let temps_pending = m.temperatures.is_empty();
        let temp_info = |cat: &str| -> String {
            temp_groups
                .get(cat)
                .map(|v| {
                    let avg = v.iter().sum::<f32>() / v.len() as f32;
                    let hmin = self.temp_min.get(cat).copied().unwrap_or(avg);
                    let hmax = self.temp_max.get(cat).copied().unwrap_or(avg);
                    format!("{:.0}°C ({:.0}–{:.0})", avg, hmin, hmax)
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

        let inline_cats_list = ["CPU", "GPU", "Memory", "SSD", "Battery"];
        let has_remaining_temps = temp_groups
            .keys()
            .any(|k| !inline_cats_list.contains(&k.as_str()));
        let last_section = if has_remaining_temps {
            "temps"
        } else {
            "peripherals"
        };
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

        // ── Battery (standalone row before the tree)
        if m.battery.present {
            let batt_w = s.battery.get();
            let display_w = if m.battery.charging {
                batt_w.abs()
            } else {
                -batt_w.abs()
            };
            let charge_status = match (m.battery.charging, m.battery.time_remaining_min) {
                (true, t) if t > 0 => format!("full in {}h {:02}m", t / 60, t % 60),
                (true, _) => "charging…".into(),
                (false, t) if t > 0 => format!("{}h {:02}m remaining", t / 60, t % 60),
                _ => "calc…".into(),
            };
            let batt_label = format!("Battery {:.0}% ({})", m.battery.percent, charge_status);
            let batt_style = if m.battery.charging {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(power_color(batt_w.abs()))
            };
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

        rows.push(TreeRow::separator());

        // ── Root: machine name with system total
        let sys_wh = w.cpu
            + w.gpu
            + w.ane
            + w.dram
            + w.gpu_sram
            + w.ssd
            + w.display
            + w.keyboard
            + w.audio
            + w.fans
            + w.wifi
            + w.bluetooth;
        rows.push(TreeRow::pw(
            "system",
            None,
            "",
            &self.machine_name,
            s.sys.get(),
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
            let soc_wh = w.cpu + w.gpu + w.ane + w.dram + w.gpu_sram;
            let e_count: usize = m.soc.ecpu_clusters.iter().map(|c| c.cores.len()).sum();
            let p_count = m.soc.pcpu_cluster.cores.len();
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
            rows.push(TreeRow::pw_full(
                "cpu",
                Some("soc"),
                &format!("{}├─ ", cp),
                &format!("CPU ({} cores)", e_count + p_count),
                s.cpu.get(),
                w.cpu,
                "",
                &temp_info("CPU"),
                Style::default(),
                pin("cpu"),
            ));
            rows.push(TreeRow::pw_full(
                "ecpu",
                Some("cpu"),
                &format!("{}│  ├─ ", cp),
                &format!("E-Cores ({})", e_count),
                s.ecpu.get(),
                w.ecpu,
                &fmt_freq(s.ecpu_freq.get()),
                "",
                DIM,
                pin("ecpu"),
            ));
            rows.push(TreeRow::pw_full(
                "pcpu",
                Some("cpu"),
                &format!("{}│  └─ ", cp),
                &format!("P-Cores ({})", p_count),
                s.pcpu.get(),
                w.pcpu,
                &fmt_freq(s.pcpu_freq.get()),
                "",
                DIM,
                pin("pcpu"),
            ));
            let gpu_name = if m.gpu_cores > 0 {
                format!("GPU ({} cores)", m.gpu_cores)
            } else {
                "GPU".into()
            };
            rows.push(TreeRow::pw_full(
                "gpu",
                Some("soc"),
                &format!("{}├─ ", cp),
                &gpu_name,
                s.gpu.get(),
                w.gpu,
                &fmt_freq(s.gpu_freq.get()),
                &temp_info("GPU"),
                Style::default(),
                pin("gpu"),
            ));
            rows.push(TreeRow::pw(
                "ane",
                Some("soc"),
                &format!("{}├─ ", cp),
                "ANE",
                s.ane.get(),
                w.ane,
                Style::default(),
                pin("ane"),
            ));
            let dram_name = if m.dram_gb > 0 {
                format!("DRAM ({} GB)", m.dram_gb)
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
                &format!("{}└─ ", cp),
                "GPU SRAM (SLC)",
                s.gpu_sram.get(),
                w.gpu_sram,
                Style::default(),
                pin("gpu_sram"),
            ));
        }

        // ── SSD
        rows.push(TreeRow::pw_full(
            "ssd",
            Some("system"),
            &t("ssd"),
            "SSD (NVMe)",
            s.ssd.get(),
            w.ssd,
            "",
            &temp_info("SSD"),
            BOLD,
            pin("ssd"),
        ));

        // ── Display
        if m.display.brightness_pct > 0.0 {
            let name = if m.display.nits > 0.0 {
                format!(
                    "Display ({:.0}% brightness, {:.0} nits)",
                    m.display.brightness_pct, m.display.nits
                )
            } else {
                format!("Display ({:.0}% brightness)", m.display.brightness_pct)
            };
            rows.push(TreeRow::pw(
                "display",
                Some("system"),
                &t("display"),
                &name,
                s.display.get(),
                w.display,
                BOLD,
                pin("display"),
            ));
        } else {
            rows.push(TreeRow::pw(
                "display",
                Some("system"),
                &t("display"),
                "Display (pending…)",
                0.0,
                0.0,
                PENDING,
                pin("display"),
            ));
        }

        // ── Keyboard
        if m.keyboard.brightness_pct > 0.0 || m.keyboard.estimated_power_w > 0.0 {
            rows.push(TreeRow::pw(
                "keyboard",
                Some("system"),
                &t("keyboard"),
                &format!("Keyboard ({:.0}% brightness)", m.keyboard.brightness_pct),
                s.keyboard.get(),
                w.keyboard,
                BOLD,
                pin("keyboard"),
            ));
        } else {
            rows.push(TreeRow::pw(
                "keyboard",
                Some("system"),
                &t("keyboard"),
                "Keyboard (pending…)",
                0.0,
                0.0,
                PENDING,
                pin("keyboard"),
            ));
        }

        // ── Audio
        let audio_status = match (m.audio.device_active, m.audio.playing, m.audio.volume_pct) {
            (false, _, _) => "off".into(),
            (_, true, Some(v)) => format!(
                "{:.0}%{}",
                v,
                if m.audio.muted { " muted" } else { ", playing" }
            ),
            (_, true, None) => "playing".into(),
            (_, false, _) => "idle".into(),
        };
        rows.push(TreeRow::pw(
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
            rows.push(TreeRow::pw(
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
            rows.push(TreeRow::pw(
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
                TreeRow::pw(
                    fan_key(i),
                    Some("fans"),
                    &pfx,
                    &format!("{} {:.0}/{:.0} RPM", fan.name, fan.actual_rpm, fan.max_rpm),
                    fan.estimated_power_w,
                    fan_wh_val,
                    Style::default(),
                    pin(fan_key(i)),
                )
            }));
        }

        // ── Peripherals
        let pc = c("peripherals");
        rows.push(TreeRow::pw(
            "peripherals",
            Some("system"),
            &t("peripherals"),
            "Peripherals",
            s.wifi.get() + s.bluetooth.get(),
            w.wifi + w.bluetooth,
            BOLD,
            pin("peripherals"),
        ));

        let (wifi_name, wifi_style) = match (m.wifi.connected, m.wifi.phy_mode.is_empty()) {
            (true, _) => (
                format!("WiFi ({} dBm, {})", m.wifi.rssi_dbm, m.wifi.phy_mode),
                Style::default(),
            ),
            (false, true) => ("WiFi (scanning…)".into(), PENDING),
            (false, false) => ("WiFi (off)".into(), Style::default()),
        };
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

        let has_traffic =
            s.net_down.get() > 0.0 || s.net_up.get() > 0.0 || self.wh.net_down_bytes > 0.0;
        if m.wifi.connected || has_traffic {
            rows.push(TreeRow::info(
                Some("wifi"),
                &format!("{}│  ├─ ", pc),
                "↓ Download",
                &human_rate(s.net_down.get() as f64),
                &human_bytes(self.wh.net_down_bytes),
                DIM,
            ));
            rows.push(TreeRow::info(
                Some("wifi"),
                &format!("{}│  └─ ", pc),
                "↑ Upload",
                &human_rate(s.net_up.get() as f64),
                &human_bytes(self.wh.net_up_bytes),
                DIM,
            ));
        }

        let bt_name = if !m.bluetooth_devices.is_empty() {
            format!("Bluetooth ({} devices)", m.bluetooth_devices.len())
        } else {
            "Bluetooth".into()
        };
        rows.push(TreeRow::pw(
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
                .map(|(l, p)| format!("{}: {}", l, p))
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
            rows.push(TreeRow::info(
                Some("peripherals"),
                &format!("{}└─ ", pc),
                &format!("USB ({} devices)", m.usb_devices.len()),
                "",
                "",
                Style::default(),
            ));
            rows.extend(m.usb_devices.iter().enumerate().map(|(i, d)| {
                let pfx = if i == m.usb_devices.len() - 1 {
                    format!("{}   └─ ", pc)
                } else {
                    format!("{}   ├─ ", pc)
                };
                let pwr = d.power_ma.map(|p| format!("{} mA", p)).unwrap_or_default();
                TreeRow::info(
                    Some("peripherals"),
                    &pfx,
                    &format!("{} {:04x}:{:04x}", d.name, d.vendor_id, d.product_id),
                    &pwr,
                    "",
                    DIM,
                )
            }));
        }

        // ── Temperatures (part of the tree, before Software)
        let inline_cats = ["CPU", "GPU", "Memory", "SSD", "Battery"];
        let remaining: Vec<_> = temp_groups
            .iter()
            .filter(|(k, _)| !inline_cats.contains(&k.as_str()))
            .collect();
        if !remaining.is_empty() {
            let tc = c("temps");
            rows.push(TreeRow::header(
                Some("system"),
                &t("temps"),
                "Temperatures",
                "",
                "Now (min–max)",
            ));
            rows.extend(remaining.iter().enumerate().map(|(i, (cat, vals))| {
                let pfx = if i == remaining.len() - 1 {
                    format!("{}└─ ", tc)
                } else {
                    format!("{}├─ ", tc)
                };
                let avg = vals.iter().sum::<f32>() / vals.len() as f32;
                let hmin = self.temp_min.get(*cat).copied().unwrap_or(avg);
                let hmax = self.temp_max.get(*cat).copied().unwrap_or(avg);
                let color = match avg {
                    a if a > 90.0 => Color::Red,
                    a if a > 70.0 => Color::Yellow,
                    _ => Color::White,
                };
                let mut r = TreeRow::info(
                    Some("temps"),
                    &pfx,
                    &format!("{} ({})", cat, vals.len()),
                    "",
                    &format!("{:.0}°C ({:.0}–{:.0})", avg, hmin, hmax),
                    Style::default().fg(color),
                );
                r.current_style = Style::default().fg(color);
                r
            }));
        }

        // ── Software (standalone collapsible section after the tree)
        rows.push(TreeRow::separator());
        let all_sw_energy = (m.all_procs_energy_mj - self.proc_baseline.values().sum::<f64>()).max(0.0);
        // Dynamic limit: fill available space, minimum 10
        let non_sw_rows = rows.len() + 3; // separator + header + footer + borders
        let proc_limit = ((self.term_height as usize).saturating_sub(non_sw_rows)).max(10);
        {
            let mut sw_row = TreeRow::pw(
                "software", None, "",
                &format!("Software (filter: top {} by total)", proc_limit),
                m.all_procs_power_w + 0.0,
                all_sw_energy / 3600.0 / 1000.0,
                BOLD, pin("software"),
            );
            sw_row.label_style = BOLD;
            rows.push(sw_row);
        }
        if m.top_processes.is_empty() {
            rows.push(TreeRow::info(Some("software"), "", "(collecting…)", "", "", PENDING));
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
        let proc_row_keys: Vec<&'static str> = display_procs.iter()
            .map(|(p, _)| proc_key(&mut self.proc_keys, p.pid))
            .collect();
        rows.extend(display_procs.iter().enumerate().map(|(i, (p, adj_mj))| {
            let pfx = if i == display_procs.len() - 1 {
                "└─ "
            } else {
                "├─ "
            };
            let dead = p.power_w == 0.0 && *adj_mj > 0.0;
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
                key, Some("software"), pfx, &label,
                p.power_w, *adj_mj / 3600.0 / 1000.0,
                Style::default().fg(color),
                self.pinned.contains(&key),
            );
            r.current_style = Style::default().fg(color);
            r
        }));

        rows
    }

    // ── Two-pass buffer renderer ────────────────────────────────────────────

    fn draw_tree_buf(&mut self, f: &mut Frame, area: Rect, rows: &[&TreeRow], all_rows: &[TreeRow]) {
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
        let spark_w = if inner.width > 90 { (inner.width - 90 - 1).min(60) } else { 0 };
        let tot_x = right.saturating_sub(COL_TOT).saturating_sub(spark_w).saturating_sub(spark_gap);
        let cur_x = tot_x.saturating_sub(COL_CUR);
        let tmp_x = cur_x.saturating_sub(COL_TEMP);
        let frq_x = tmp_x.saturating_sub(COL_FREQ);
        let spark_x = right.saturating_sub(spark_w);

        buf.set_string(inner.x + 2, hdr_y, "Component", BOLD);
        right_str(buf, frq_x, hdr_y, COL_FREQ, "Freq", BOLD);
        right_str(buf, tmp_x, hdr_y, COL_TEMP, "Temp", BOLD);
        right_str(buf, cur_x, hdr_y, COL_CUR, "Current", BOLD);
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
                buf.set_string(frq_x, y, &" ".repeat(COL_FREQ as usize), Style::default());
                right_str(buf, frq_x, y, COL_FREQ, &row.freq, DIM);
            }
            if !row.temp.is_empty() {
                buf.set_string(tmp_x, y, &" ".repeat(COL_TEMP as usize), Style::default());
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
                        for (ci, &val) in visible.iter().enumerate() {
                            let x = spark_x + (w - visible.len() + ci) as u16;
                            let level = (val / vis_max * 7.0).round() as usize;
                            let ch = SPARK_CHARS[level.min(7)];
                            let color = power_color(val as f32);
                            buf.set_string(x, y, &ch.to_string(), Style::default().fg(color));
                        }
                    }
                }
            }

            // Cursor highlight (background only, preserves text colors)
            if abs_idx == self.cursor {
                let row_rect = Rect::new(inner.x, y, inner.width, 1);
                buf.set_style(row_rect, CURSOR_BG);
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
                Style::default().fg(Color::White)
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

            let vals: Vec<u64> = if scale_max > 0.0 {
                visible_data
                    .iter()
                    .map(|&v| (v / scale_max * 1000.0).round() as u64)
                    .collect()
            } else {
                vec![0]
            };

            let scale_h = inner[0].height;
            let scale_lines: Vec<Line> = (0..scale_h)
                .map(|row| {
                    if row == 0 {
                        Line::from(Span::styled(format!("{:>5.1}", scale_max), DIM))
                    } else if row == scale_h / 2 {
                        Line::from(Span::styled(format!("{:>5.1}", scale_max / 2.0), DIM))
                    } else if row == scale_h - 1 {
                        Line::from(Span::styled("  0.0", DIM))
                    } else {
                        Line::from("")
                    }
                })
                .collect();
            f.render_widget(Paragraph::new(scale_lines), inner[0]);

            let spark = Sparkline::default()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(DIM)
                        .title(Span::styled(
                            format!(
                                " {} — {:.3} W{} ",
                                self.labels.get(key).map(|s| s.as_str()).unwrap_or(key),
                                current,
                                pin_icon
                            ),
                            title_style,
                        )),
                )
                .data(&vals)
                .max(1000)
                .style(Style::default().fg(power_color(current as f32)));
            f.render_widget(spark, inner[1]);
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
            Span::styled("↑/↓", Style::default().fg(Color::Yellow)),
            Span::raw(" select  "),
            Span::styled("←/→", Style::default().fg(Color::Yellow)),
            Span::raw(" fold  "),
            Span::styled("space", Style::default().fg(Color::Yellow)),
            Span::raw(" pin    "),
            Span::styled("■", Style::default().fg(Color::Green)),
            Span::raw("<1W "),
            Span::styled("■", Style::default().fg(Color::Yellow)),
            Span::raw("<5W "),
            Span::styled("■", Style::default().fg(Color::Rgb(255, 165, 0))),
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
    let steps = [0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0];
    steps
        .iter()
        .copied()
        .find(|&s| s >= max_val)
        .unwrap_or(max_val.ceil().max(1.0))
}

fn fan_key(index: usize) -> &'static str {
    const KEYS: [&str; 8] = ["fan0", "fan1", "fan2", "fan3", "fan4", "fan5", "fan6", "fan7"];
    KEYS.get(index).copied().unwrap_or("fan0")
}

fn proc_key(cache: &mut std::collections::HashMap<i32, &'static str>, pid: i32) -> &'static str {
    *cache.entry(pid).or_insert_with(|| Box::leak(format!("pid.{}", pid).into_boxed_str()))
}

fn read_machine_name() -> String {
    let chip = std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .and_then(|o| Some(String::from_utf8_lossy(&o.stdout).trim().to_string()))
        .filter(|s| !s.is_empty());
    let model = std::process::Command::new("sysctl")
        .args(["-n", "hw.model"])
        .output()
        .ok()
        .and_then(|o| Some(String::from_utf8_lossy(&o.stdout).trim().to_string()))
        .filter(|s| !s.is_empty());
    match (chip, model) {
        (Some(c), Some(m)) => format!("{} ({})", c, m),
        (Some(c), None) => c,
        (None, Some(m)) => m,
        _ => "Mac".into(),
    }
}
