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
use std::sync::mpsc;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = env!("CARGO_PKG_NAME"), version, about = "Apple Silicon Power Monitor TUI")]
struct CliArgs {
    /// Sampling interval in milliseconds
    #[arg(long, default_value_t = 250)]
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
    let interval = args.interval;
    let json_mode = args.json;

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

    let (tx, rx) = mpsc::sync_channel::<Metrics>(2);

    // Sampler spawns independent threads per source, all update shared state.
    // This thread just snapshots and sends to the TUI at the desired interval.
    std::thread::spawn(move || {
        let sampler = Sampler::new(interval);
        loop {
            std::thread::sleep(Duration::from_millis(interval));
            let m = sampler.snapshot();
            if tx.send(m).is_err() {
                break;
            }
        }
    });

    if json_mode {
        run_json(rx)
    } else {
        run_tui(rx)
    }
}

fn run_json(rx: mpsc::Receiver<Metrics>) -> Result<()> {
    // No SIGINT handler on purpose: JSON mode has nothing to clean up, so the
    // default disposition (die at once, even while blocked on a slow reader)
    // is what we want. A handler that calls `process::exit` is not
    // async-signal-safe: it aborts when it interrupts a write to stdout.
    let mut out = stdout().lock();
    for m in rx {
        if let Err(e) = write_json_line(&mut out, &m) {
            // A reader that has seen enough and closed the pipe
            // (`macpow --json | head -1`) is a normal way to stop.
            return match e.kind() {
                io::ErrorKind::BrokenPipe => Ok(()),
                _ => Err(e.into()),
            };
        }
    }
    Ok(())
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

fn run_tui(rx: mpsc::Receiver<Metrics>) -> Result<()> {
    if unsafe { libc::isatty(libc::STDOUT_FILENO) } == 0 {
        anyhow::bail!("TUI requires a real terminal. Use --json for piped output.");
    }
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    stdout().execute(crossterm::event::EnableMouseCapture)?;

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        let backend = CrosstermBackend::new(stdout());
        let mut terminal = Terminal::new(backend)?;
        let mut app = App::new();

        loop {
            while let Ok(m) = rx.try_recv() {
                app.update(m);
            }
            terminal.draw(|f| app.draw(f))?;
            if event::poll(Duration::from_millis(app.poll_interval_ms()))? {
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
