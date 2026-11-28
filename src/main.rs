mod app;

use macpow::metrics::Sampler;
use macpow::types::Metrics;

use anyhow::Result;
use app::App;
use clap::Parser;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, stdout, Write};
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
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

    /// Output JSON Lines to stdout instead of the TUI: one compact object per
    /// line, one line per interval
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
        // An error, not just a message: a script that collects diagnostics
        // must be able to tell a failed dump from a successful one.
        let ior = macpow::ioreport::IOReportSampler::new().map_err(|e| {
            anyhow::anyhow!(
                "Failed to initialize IOReport: {e}\n\
                 This Mac may not support the required IOReport channels."
            )
        })?;
        ior.dump_channels();
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
// use one, so it always shows the newest published state, never a queued one:
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
    let _ = stdout().execute(DisableMouseCapture);
    let _ = disable_raw_mode();
    let _ = stdout().execute(LeaveAlternateScreen);
}

/// The terminal in TUI mode: raw input, alternate screen, mouse reporting.
/// Dropping it gives the terminal back, so every way out of `run_tui` (quit
/// key, signal, `?` on an error) leaves a usable shell.
struct TuiMode;

impl TuiMode {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        // Raw mode is on: from here an early return has to undo it.
        let mode = TuiMode;
        stdout().execute(EnterAlternateScreen)?;
        stdout().execute(EnableMouseCapture)?;
        Ok(mode)
    }
}

impl Drop for TuiMode {
    fn drop(&mut self) {
        // While unwinding, the panic hook has already restored the terminal
        // and the message is on the normal screen. Leaving the alternate
        // screen a second time would move the cursor back on top of it.
        if !std::thread::panicking() {
            restore_terminal();
        }
    }
}

/// Makes a panic of the UI thread readable: the terminal is given back first,
/// then the previous hook prints. Printed while the alternate screen is up,
/// the message would disappear together with that screen.
fn install_panic_hook() {
    let ui_thread = std::thread::current().id();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // A sampler thread can panic as well, and the TUI keeps running then.
        // Only a panic on the UI thread ends it.
        if std::thread::current().id() == ui_thread {
            restore_terminal();
        }
        previous(info);
    }));
}

/// Set by the signal handler when the process is asked to stop. The TUI loop
/// polls it and leaves through its normal exit, which restores the terminal.
static TERMINATE: AtomicBool = AtomicBool::new(false);

/// How long the TUI waits for input before it looks at `TERMINATE` again.
/// crossterm restarts its poll when a signal interrupts it, so the signal by
/// itself does not end the wait.
const SIGNAL_CHECK: Duration = Duration::from_millis(100);

extern "C" fn request_terminate(_signal: libc::c_int) {
    TERMINATE.store(true, Ordering::Relaxed);
}

/// Routes SIGINT, SIGTERM and SIGHUP to `TERMINATE`. Their default action
/// kills the process on the spot and leaves the shell in raw mode, on the
/// alternate screen and with mouse reporting on.
fn install_terminate_handlers() {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: `request_terminate` has the signature signal(2) expects and
        // only stores to an atomic, which is async-signal-safe.
        unsafe {
            libc::signal(signal, request_terminate as *const () as libc::sighandler_t);
        }
    }
}

/// Waits for terminal input until `deadline`. Returns `None` when the deadline
/// passes or a termination signal arrives first.
fn next_event(deadline: Instant) -> io::Result<Option<Event>> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        // Polled at least once even when the deadline has already passed, so
        // that slow frames cannot starve the keyboard.
        if event::poll(left.min(SIGNAL_CHECK))? {
            return event::read().map(Some);
        }
        // `left <= SIGNAL_CHECK` means the whole remainder has been waited out.
        if left <= SIGNAL_CHECK || TERMINATE.load(Ordering::Relaxed) {
            return Ok(None);
        }
    }
}

fn run_tui(interval_ms: u64) -> Result<()> {
    if unsafe { libc::isatty(libc::STDOUT_FILENO) } == 0 {
        anyhow::bail!("TUI requires a real terminal. Use --json for piped output.");
    }
    // Before the terminal is touched, so that there is no moment in which a
    // signal still kills the process with the terminal in TUI mode.
    install_terminate_handlers();
    let _tui_mode = TuiMode::enter()?;
    install_panic_hook();

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    let mut app = App::new(interval_ms);
    // Show the still empty screen before starting the sampler: its start-up
    // reads take several hundred milliseconds.
    terminal.draw(|f| app.draw(f))?;
    let sampler = start_sampler(interval_ms);
    let mut last_update = Instant::now();

    while !TERMINATE.load(Ordering::Relaxed) {
        let refresh = app.refresh_interval();
        if last_update.elapsed() >= refresh {
            app.update(sampler.snapshot());
            last_update = Instant::now();
        }
        terminal.draw(|f| app.draw(f))?;
        // Wait for input only until the next frame is due.
        match next_event(last_update + refresh)? {
            Some(Event::Key(key)) if key.kind == KeyEventKind::Press && app.handle_key(key) => {
                break;
            }
            Some(Event::Mouse(mouse)) => app.handle_mouse(mouse),
            _ => {}
        }
    }
    Ok(())
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
