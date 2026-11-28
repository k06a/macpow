use std::collections::VecDeque;
use std::time::Instant;

/// Time-weighted Simple Moving Average.
///
/// The values pushed here (power, data rates, frequencies) are each an average
/// over the time since the previous reading, so a sample is weighted by the
/// interval that precedes it. `get()` is the mean over the last `window_secs`
/// of data, ending at the newest sample:
///
/// - The newest sample counts in full as soon as it is pushed.
/// - The oldest retained sample only marks where the next interval starts:
///   nothing says how long it was measured over. A lone sample is returned
///   as is.
/// - An interval that straddles the start of the window counts only for the
///   part inside the window.
/// - A window of 0 means no smoothing: `get()` returns the newest sample.
/// - A gap gets no special treatment. The sample that ends it is weighted by
///   the whole gap like any other interval, so after a gap of `window_secs`
///   or more it is the result on its own. Sleep is not a gap: `Instant` does
///   not advance while the Mac is asleep.
/// - `get()` does not read the clock, so the result changes only on a push.
pub struct TimeSma {
    /// `(time of push, value)`, oldest first.
    buf: VecDeque<(Instant, f32)>,
    /// Never negative or NaN, see `set_window`.
    window_secs: f64,
}

impl TimeSma {
    pub fn new(window_secs: f64) -> Self {
        let mut sma = Self {
            buf: VecDeque::new(),
            window_secs: 0.0,
        };
        sma.set_window(window_secs);
        sma
    }

    pub fn push(&mut self, value: f32) {
        self.push_at(Instant::now(), value);
    }

    /// `push` with the time passed in, so that tests need neither the clock
    /// nor sleeping. `at` must not be earlier than the previous sample's;
    /// `Instant::now()` never is.
    pub fn push_at(&mut self, at: Instant, value: f32) {
        self.buf.push_back((at, value));
        self.trim();
    }

    pub fn set_window(&mut self, secs: f64) {
        // `max` turns a negative or NaN window into 0 (no smoothing). A NaN
        // would compare false with every age, so nothing would ever be trimmed.
        self.window_secs = secs.max(0.0);
        self.trim();
    }

    pub fn get(&self) -> f32 {
        let Some(&(newest, latest)) = self.buf.back() else {
            return 0.0;
        };
        let mut weighted_sum = 0.0f64;
        let mut covered_secs = 0.0f64;
        // The oldest sample only starts the first interval.
        let mut start_age = age(newest, self.buf[0].0);
        for &(end, value) in self.buf.iter().skip(1) {
            let end_age = age(newest, end);
            // Only the part of the interval inside the window counts.
            let secs = start_age.min(self.window_secs) - end_age;
            if secs > 0.0 {
                weighted_sum += value as f64 * secs;
                covered_secs += secs;
            }
            start_age = end_age;
        }
        if covered_secs > 0.0 {
            (weighted_sum / covered_secs) as f32
        } else {
            // A lone sample, no smoothing, or no time between the samples.
            latest
        }
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Drops the samples the window no longer needs. One sample at or before
    /// the start of the window stays: the next sample's interval begins there.
    fn trim(&mut self) {
        let Some(&(newest, _)) = self.buf.back() else {
            return;
        };
        while self.buf.len() > 1 && age(newest, self.buf[1].0) >= self.window_secs {
            self.buf.pop_front();
        }
    }
}

/// Seconds from `t` to `newest`, 0 if `t` is not earlier.
fn age(newest: Instant, t: Instant) -> f64 {
    newest.saturating_duration_since(t).as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Pushes `(milliseconds after origin, value)` samples; no test sleeps.
    fn feed(sma: &mut TimeSma, origin: Instant, samples: &[(u64, f32)]) {
        for &(ms, value) in samples {
            sma.push_at(origin + Duration::from_millis(ms), value);
        }
    }

    fn sma_of(window_secs: f64, samples: &[(u64, f32)]) -> TimeSma {
        let mut sma = TimeSma::new(window_secs);
        feed(&mut sma, Instant::now(), samples);
        sma
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1e-4,
            "got {actual}, expected {expected}"
        );
    }

    #[test]
    fn empty_sma_returns_zero() {
        let sma = TimeSma::new(5.0);
        assert_eq!(sma.get(), 0.0);
    }

    #[test]
    fn lone_sample_is_returned_as_is() {
        assert_eq!(sma_of(5.0, &[(0, 42.0)]).get(), 42.0);
    }

    #[test]
    fn zero_window_returns_latest_value() {
        let sma = sma_of(0.0, &[(0, 1.0), (1000, 3.5)]);
        assert_eq!(sma.get(), 3.5);
        assert_eq!(sma.buf.len(), 1);
    }

    #[test]
    fn newest_sample_counts_as_soon_as_it_is_pushed() {
        // The reading of 20 covers the 200 ms before it. Weighting by the time
        // until the next sample gave it no weight here and returned 10.
        assert_eq!(sma_of(5.0, &[(0, 10.0), (200, 20.0)]).get(), 20.0);
    }

    #[test]
    fn push_stamps_samples_with_the_real_clock() {
        let mut sma = TimeSma::new(5.0);
        sma.push(10.0);
        sma.push(20.0);
        // Whatever time passed in between, only 20 has a known interval.
        assert_eq!(sma.get(), 20.0);
    }

    #[test]
    fn samples_are_weighted_by_the_interval_before_them() {
        // 20 over 1 s, then 40 over 3 s; 10 only marks the start.
        let sma = sma_of(60.0, &[(0, 10.0), (1000, 20.0), (4000, 40.0)]);
        assert_close(sma.get(), (20.0 * 1.0 + 40.0 * 3.0) / 4.0);
    }

    #[test]
    fn constant_input_gives_the_same_constant() {
        let samples: Vec<(u64, f32)> = (0..40).map(|i| (i * 250, 7.3)).collect();
        assert_eq!(sma_of(5.0, &samples).get(), 7.3);
    }

    #[test]
    fn samples_older_than_the_window_do_not_count() {
        let mut samples = vec![(0, 1000.0), (1000, 1000.0)];
        samples.extend((2..=7).map(|s| (s * 1000, 10.0)));
        let sma = sma_of(5.0, &samples);
        assert_eq!(sma.get(), 10.0);
        // The sample at 2 s is the start of the window, then five inside it.
        assert_eq!(sma.buf.len(), 6);
    }

    #[test]
    fn interval_straddling_the_window_start_is_clipped() {
        // The window is 3 s..8 s: 10 counts for 3 s..4 s, 20 for 4 s..8 s.
        let sma = sma_of(5.0, &[(0, 100.0), (4000, 10.0), (8000, 20.0)]);
        assert_close(sma.get(), (10.0 * 1.0 + 20.0 * 4.0) / 5.0);
    }

    #[test]
    fn buffer_stays_bounded_by_the_window() {
        let samples: Vec<(u64, f32)> = (0..10_000).map(|ms| (ms, 1.0)).collect();
        let sma = sma_of(1.0, &samples);
        // 1000 samples newer than the window start, plus the one that marks it.
        assert_eq!(sma.buf.len(), 1001);
    }

    #[test]
    fn set_window_shrinks_to_the_newest_data() {
        let origin = Instant::now();
        let mut sma = TimeSma::new(10.0);
        let samples = [
            (0, 10.0),
            (2000, 20.0),
            (4000, 30.0),
            (6000, 40.0),
            (8000, 50.0),
        ];
        feed(&mut sma, origin, &samples);
        assert_close(sma.get(), 35.0);

        sma.set_window(4.0);
        assert_close(sma.get(), (40.0 * 2.0 + 50.0 * 2.0) / 4.0);
        // Not on a sample boundary: 40 counts for 5 s..6 s only.
        sma.set_window(3.0);
        assert_close(sma.get(), (40.0 * 1.0 + 50.0 * 2.0) / 3.0);

        sma.set_window(0.0);
        assert_eq!(sma.get(), 50.0);
        assert_eq!(sma.buf.len(), 1);

        // Growing the window does not bring dropped samples back; the one
        // that is left marks the start of the next interval.
        sma.set_window(10.0);
        assert_eq!(sma.get(), 50.0);
        feed(&mut sma, origin, &[(10_000, 70.0)]);
        assert_eq!(sma.get(), 70.0);
        feed(&mut sma, origin, &[(11_000, 10.0)]);
        assert_close(sma.get(), (70.0 * 2.0 + 10.0 * 1.0) / 3.0);
    }

    #[test]
    fn sample_after_a_long_gap_covers_the_whole_window() {
        let origin = Instant::now();
        let mut sma = TimeSma::new(5.0);
        let before_gap = [(0, 10.0), (1000, 10.0), (2000, 10.0)];
        feed(&mut sma, origin, &before_gap);
        feed(&mut sma, origin, &[(62_000, 80.0)]);
        assert_eq!(sma.get(), 80.0);
        // Everything before the gap is gone except the start of the interval.
        assert_eq!(sma.buf.len(), 2);

        // One second later the gap sample still fills the other 4 s.
        feed(&mut sma, origin, &[(63_000, 20.0)]);
        assert_close(sma.get(), (80.0 * 4.0 + 20.0 * 1.0) / 5.0);
    }

    #[test]
    fn samples_without_time_between_them_return_the_newest() {
        assert_eq!(sma_of(5.0, &[(1000, 10.0), (1000, 20.0)]).get(), 20.0);
        // Time going backwards must not panic either.
        assert_eq!(sma_of(5.0, &[(1000, 10.0), (0, 20.0)]).get(), 20.0);
    }

    #[test]
    fn negative_or_nan_window_means_no_smoothing() {
        for window in [-1.0, f64::NEG_INFINITY, f64::NAN] {
            let origin = Instant::now();
            let mut sma = TimeSma::new(window);
            feed(&mut sma, origin, &[(0, 1.0), (1000, 3.5)]);
            assert_eq!(sma.get(), 3.5, "window {window}");
            assert_eq!(sma.buf.len(), 1, "window {window}");

            sma.set_window(5.0);
            feed(&mut sma, origin, &[(2000, 5.0), (3000, 7.0)]);
            sma.set_window(window);
            assert_eq!(sma.get(), 7.0, "window {window}");
            assert_eq!(sma.buf.len(), 1, "window {window}");
        }
    }

    #[test]
    fn clear_forgets_all_samples() {
        let origin = Instant::now();
        let mut sma = TimeSma::new(5.0);
        feed(&mut sma, origin, &[(0, 10.0), (1000, 20.0)]);
        sma.clear();
        assert_eq!(sma.get(), 0.0);
        feed(&mut sma, origin, &[(2000, 30.0)]);
        assert_eq!(sma.get(), 30.0);
    }
}
