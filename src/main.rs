mod app;

use macpow::metrics::Sampler;
use macpow::types::{CliArgs, Metrics};

use anyhow::Result;
use app::App;
use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::stdout;
use std::sync::mpsc;
use std::time::Duration;

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
    let _ = stdout().execute(PopKeyboardEnhancementFlags);
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
    let _ = stdout().execute(PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::all()));

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
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if app.handle_key(key) {
                            break;
                        }
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
