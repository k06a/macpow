mod app;
mod apple_sampler;

use macpow::ioreport::IOReportSampler;
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
use std::io::{stdin, stdout, IsTerminal};
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

    /// Skip starting Apple's per-core CPU power sampler in the terminal UI
    #[arg(long)]
    no_powermetrics: bool,

    /// Dump all IOReport channel names and exit (for diagnostics)
    #[arg(long)]
    dump: bool,

    /// Check whether each CPU core's energy counter updates over three seconds
    #[arg(long, conflicts_with_all = ["dump", "dump_smc", "json"])]
    check_cpu_cores: bool,

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

    if args.check_cpu_cores {
        return macpow::ioreport::IOReportSampler::new()?.check_cpu_core_counters();
    }

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

    if !json_mode && !stdout().is_terminal() {
        anyhow::bail!("TUI requires a real terminal. Use --json for piped output.");
    }
    let _apple_sampler = if !json_mode
        && !args.no_powermetrics
        && stdin().is_terminal()
        // Only the foreground job may ask for a terminal password.
        && unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) == libc::getpgrp() }
        && IOReportSampler::needs_cpu_power_sampler()
    {
        apple_sampler::start()
    } else {
        None
    };

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
    unsafe {
        libc::signal(
            libc::SIGINT,
            sigint_handler as *const () as libc::sighandler_t,
        );
    }
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(m) => println!("{}", serde_json::to_string_pretty(&m)?),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn restore_terminal() {
    let _ = stdout().execute(crossterm::event::DisableMouseCapture);
    let _ = disable_raw_mode();
    let _ = stdout().execute(LeaveAlternateScreen);
}

fn run_tui(rx: mpsc::Receiver<Metrics>) -> Result<()> {
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

extern "C" fn sigint_handler(_: libc::c_int) {
    std::process::exit(0);
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
