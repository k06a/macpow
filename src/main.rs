mod app;

use macpow::metrics::Sampler;
use macpow::types::Metrics;

use anyhow::Result;
use app::App;
use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, stdout, Write};
use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

/// Accepted `--interval` values, in milliseconds. The sources do not change
/// faster than every 100 ms, so anything lower only repeats the same numbers
/// (unlimited, `--json --interval 0` wrote about 190 MB/s of duplicates). The
/// upper bound keeps a typo from looking like a hung program.
const INTERVAL_MS: std::ops::RangeInclusive<u64> = 100..=60_000;

#[derive(Parser, Debug)]
#[command(name = env!("CARGO_PKG_NAME"), version, about = "Apple Silicon Power Monitor TUI")]
struct CliArgs {
    /// Sampling interval in milliseconds (100 to 60000)
    #[arg(long, default_value_t = 250, value_parser = clap::value_parser!(u64).range(INTERVAL_MS))]
    interval: u64,

    /// Output JSON to stdout instead of TUI
    #[arg(long)]
    json: bool,

    /// Dump all IOReport channel names and exit (for diagnostics)
    #[arg(long)]
    dump: bool,

    /// Dump every SMC key (name, type, decoded value, raw bytes) and exit.
    /// Output mirrors `iSMC raw` format for cross-reference. Useful for triaging
    /// power readings on new hardware (see GitHub issue #12).
    #[arg(long)]
    dump_smc: bool,
}

fn main() -> Result<()> {
    let args = CliArgs::parse();

    if args.dump {
        match macpow::ioreport::IOReportSampler::new() {
            Ok(ior) => ior.dump_channels(),
            Err(e) => eprintln!("Failed to initialize IOReport: {e}\nThis Mac may not support the required IOReport channels."),
        }
        return Ok(());
    }

    if args.dump_smc {
        return run_dump_smc();
    }

    if args.json {
        run_json(args.interval)
    } else {
        run_tui(args.interval)
    }
}

// Timing model, the same for both front ends.
//
// The Sampler runs one thread per data source. Each thread re-reads its source
// every `--interval` and publishes the result into shared state, and
// `Sampler::snapshot` copies that state without waiting for any of them. The
// front end owns the Sampler and takes a snapshot at the moment it is about to
// use one, so what it shows is never older than one sampling interval:
//
// - `--json` prints one snapshot every `--interval`;
// - the TUI takes one snapshot per refresh. The refresh interval starts at
//   `--interval`; the `l` key stretches it to 2x, 4x and 8x. That only slows
//   the screen down, the sources keep sampling every `--interval`.

/// Starts the sampler threads; they run until the process exits.
///
/// The Sampler is never dropped on purpose. Its `Drop` joins every source
/// thread, and each of them may be asleep for a whole interval, so quitting
/// would hang for up to `--interval` (measured: 0.96 s at 1000 ms). The threads
/// hold nothing that has to be released before the process ends.
fn start_sampler(interval_ms: u64) -> ManuallyDrop<Sampler> {
    ManuallyDrop::new(Sampler::new(interval_ms))
}

fn run_json(interval_ms: u64) -> Result<()> {
    // No SIGINT handler on purpose: JSON mode has nothing to clean up, so the
    // default disposition (die at once, even while blocked on a slow reader)
    // is what we want. A handler that calls `process::exit` is not
    // async-signal-safe: it aborts when it interrupts a write to stdout.
    let sampler = start_sampler(interval_ms);
    let mut out = stdout().lock();
    loop {
        std::thread::sleep(Duration::from_millis(interval_ms));
        if let Err(e) = write_json_line(&mut out, &sampler.snapshot()) {
            // A reader that has seen enough and closed the pipe
            // (`macpow --json | head -1`) is a normal way to stop.
            return match e.kind() {
                io::ErrorKind::BrokenPipe => Ok(()),
                _ => Err(e.into()),
            };
        }
    }
}

/// Writes one snapshot as a single line of compact JSON (JSON Lines) and
/// flushes it, so a reader gets every object as soon as it exists.
fn write_json_line(out: &mut impl Write, m: &Metrics) -> io::Result<()> {
    // Serialise into memory first: the whole line then reaches stdout in one
    // write instead of dribbling through std's 1 KB line buffer.
    let mut line = serde_json::to_vec(m)?;
    line.push(b'\n');
    out.write_all(&line)?;
    out.flush()
}

fn restore_terminal() {
    let _ = stdout().execute(crossterm::event::DisableMouseCapture);
    let _ = disable_raw_mode();
    let _ = stdout().execute(LeaveAlternateScreen);
}

fn run_tui(interval_ms: u64) -> Result<()> {
    if unsafe { libc::isatty(libc::STDOUT_FILENO) } == 0 {
        anyhow::bail!("TUI requires a real terminal. Use --json for piped output.");
    }
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    stdout().execute(crossterm::event::EnableMouseCapture)?;

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        let backend = CrosstermBackend::new(stdout());
        let mut terminal = Terminal::new(backend)?;
        let mut app = App::new(interval_ms);
        // Show the still empty screen before starting the sampler: its
        // start-up reads take several hundred milliseconds.
        terminal.draw(|f| app.draw(f))?;
        let sampler = start_sampler(interval_ms);
        let mut last_update = Instant::now();

        loop {
            let refresh = app.refresh_interval();
            if last_update.elapsed() >= refresh {
                app.update(sampler.snapshot());
                last_update = Instant::now();
            }
            terminal.draw(|f| app.draw(f))?;
            // Wait for input only until the next frame is due.
            if event::poll(refresh.saturating_sub(last_update.elapsed()))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press && app.handle_key(key) => {
                        break;
                    }
                    Event::Mouse(mouse) => {
                        app.handle_mouse(mouse);
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }));

    restore_terminal();

    match result {
        Ok(inner) => inner,
        Err(_) => anyhow::bail!("TUI panicked unexpectedly. Terminal has been restored."),
    }
}

/// Dump every SMC key in the same format as `iSMC raw`, so the output can be
/// diffed against existing dumps in `dkorunic/iSMC/reports/`.
fn run_dump_smc() -> Result<()> {
    let mut smc =
        macpow::smc::SmcConnection::open().map_err(|e| anyhow::anyhow!("SMC open failed: {e}"))?;
    let entries = smc.dump_all();
    if entries.is_empty() {
        eprintln!("SMC: no keys returned (kernel may have refused enumeration)");
        return Ok(());
    }
    for e in &entries {
        let hex = e.bytes_hex();
        let decoded = e.decoded();
        if decoded.is_empty() {
            println!("  {}  [{:<4}]  (bytes {})", e.key, e.data_type, hex);
        } else {
            println!(
                "  {}  [{:<4}]  {} (bytes {})",
                e.key, e.data_type, decoded, hex
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer whose reader has gone away.
    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn parse_interval(value: &str) -> Result<u64, clap::Error> {
        CliArgs::try_parse_from(["macpow", "--interval", value]).map(|args| args.interval)
    }

    #[test]
    fn interval_defaults_to_250_and_accepts_100_ms_to_one_minute() {
        assert_eq!(CliArgs::try_parse_from(["macpow"]).unwrap().interval, 250);
        assert_eq!(parse_interval("100").unwrap(), 100);
        assert_eq!(parse_interval("60000").unwrap(), 60_000);
    }

    #[test]
    fn interval_outside_the_range_is_a_usage_error() {
        for value in ["0", "1", "99", "60001", "18446744073709551615"] {
            let err = parse_interval(value).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ValueValidation,
                "--interval {value}"
            );
        }
    }

    #[test]
    fn l_key_steps_through_multiples_of_the_sampling_interval() {
        // The default keeps the steps the key always had.
        let mut steps = vec![250];
        for _ in 0..4 {
            steps.push(App::next_refresh_ms(250, steps[steps.len() - 1]));
        }
        assert_eq!(steps, [250, 500, 1000, 2000, 250]);

        // A custom --interval is the base of the cycle and is never undercut.
        assert_eq!(App::next_refresh_ms(300, 300), 600);
        assert_eq!(App::next_refresh_ms(300, 2400), 300);
        assert_eq!(App::next_refresh_ms(60_000, 480_000), 60_000);
    }

    #[test]
    fn json_line_is_one_compact_object_per_line() {
        let mut out = Vec::new();
        write_json_line(&mut out, &Metrics::default()).unwrap();
        write_json_line(&mut out, &Metrics::default()).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.ends_with('\n'));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per object");
        for line in lines {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(value.is_object());
        }
    }

    #[test]
    fn json_line_reports_a_closed_pipe_as_broken_pipe() {
        let err = write_json_line(&mut ClosedPipe, &Metrics::default()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }
}
