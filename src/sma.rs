use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Time-weighted Simple Moving Average.
/// Each sample is weighted by the duration it was "active" (until the next sample).
pub struct TimeSma {
    buf: VecDeque<(Instant, f32)>,
    window_secs: f64,
}

impl TimeSma {
    pub fn new(window_secs: f64) -> Self {
        Self {
            buf: VecDeque::new(),
            window_secs,
        }
    }

    pub fn push(&mut self, value: f32) {
        self.buf.push_back((Instant::now(), value));
        self.trim();
    }

    pub fn set_window(&mut self, secs: f64) {
        self.window_secs = secs;
        self.trim();
    }

    pub fn get(&self) -> f32 {
        if self.buf.is_empty() {
            return 0.0;
        }
        if self.window_secs == 0.0 || self.buf.len() <= 1 {
            return self.buf.back().map(|x| x.1).unwrap_or(0.0);
        }
        let now = Instant::now();
        let cutoff = now - Duration::from_secs_f64(self.window_secs);
        let mut weighted_sum = 0.0f64;
        let mut total_duration = 0.0f64;
        let items: Vec<_> = self.buf.iter().filter(|(t, _)| *t >= cutoff).collect();
        if items.is_empty() {
            return self.buf.back().map(|x| x.1).unwrap_or(0.0);
        }
        for i in 0..items.len() {
            let dt = if i + 1 < items.len() {
                items[i + 1].0.duration_since(items[i].0).as_secs_f64()
            } else {
                now.duration_since(items[i].0).as_secs_f64()
            };
            weighted_sum += items[i].1 as f64 * dt;
            total_duration += dt;
        }
        if total_duration > 0.0 {
            (weighted_sum / total_duration) as f32
        } else {
            self.buf.back().map(|x| x.1).unwrap_or(0.0)
        }
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    fn trim(&mut self) {
        if self.window_secs == 0.0 {
            while self.buf.len() > 1 {
                self.buf.pop_front();
            }
            return;
        }
        let cutoff = Instant::now() - Duration::from_secs_f64(self.window_secs + 1.0);
        while self.buf.front().is_some_and(|x| x.0 < cutoff) {
            self.buf.pop_front();
        }
    }
}
