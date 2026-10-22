//! `zapfast serve`: Whats Pro with no window, for a server with no display.
//!
//! It links as a device of its own and keeps the link, the archive, the
//! local bridge (src/bridge.rs) and the project analysis running, so `zapfast
//! mcp` on the same machine reads and writes as it does beside the desktop
//! app. Pairing happens on the terminal: a QR code on stderr (or in a PNG),
//! or a pairing code for a phone number. The log file records only the
//! link's state, never the QR payload or the pairing code.

use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use clap::Parser;

use crate::{
    app::{App, AppOptions},
    backend::{Command, LinkStatus, Waker},
    paths::AppDirs,
    single_instance,
};

/// Runs Whats Pro headless on a server, as a linked device of its own.
#[derive(Debug, Parser)]
#[command(name = "zapfast serve", bin_name = "zapfast serve")]
pub struct Args {
    /// Keep the archive and AI keys as private files in this folder instead
    /// of the OS keyring, which a server does not have. Sets
    /// WHATSPRO_SECRETS_DIR for the processes Whats Pro starts.
    #[arg(long, value_name = "DIR")]
    secrets_dir: Option<PathBuf>,
    /// Link with a pairing code for this phone number (with its country
    /// code, as in +5491122334455) instead of a QR code.
    #[arg(long, value_name = "E.164", value_parser = phone_digits)]
    pair_phone: Option<String>,
    /// Also save the QR code as a PNG here while linking (deleted once
    /// linked), for a terminal that cannot show it.
    #[arg(long, value_name = "PATH")]
    qr_png: Option<PathBuf>,
    /// Log more from the WhatsApp library.
    #[arg(short, long)]
    verbose: bool,
}

/// The digits of a phone number, as WhatsApp's pairing takes it.
fn phone_digits(text: &str) -> Result<String, String> {
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    let allowed = |c: char| c.is_ascii_digit() || matches!(c, '+' | ' ' | '-' | '(' | ')');
    if digits.len() < 7 || !text.chars().all(allowed) {
        return Err("enter the phone number with its country code, as in +5491122334455".into());
    }
    Ok(digits)
}

/// Runs `serve` with the command line after the program name, and returns
/// the process's exit code.
pub fn run(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> i32 {
    let args = Args::parse_from(arguments);
    match serve(args) {
        Ok(()) => 0,
        Err(error) => {
            log::error!("serve stopped: {error:#}");
            eprintln!("Whats Pro: {error:#}");
            1
        }
    }
}

fn serve(args: Args) -> anyhow::Result<()> {
    if let Some(dir) = &args.secrets_dir {
        let dir = std::path::absolute(dir)?;
        // SAFETY: nothing else runs yet; this is the process's first thread,
        // before logging, the runtime or any worker starts.
        unsafe { std::env::set_var(crate::secrets::DIR_VAR, &dir) };
    }
    // Before anything is linked or opened: a server with nowhere to keep the
    // archive key says so now, naming the flag.
    crate::secrets::file_store()?;
    let dirs = AppDirs::discover();
    dirs.ensure()?;
    let waker = Waker::default();
    let instance = match single_instance::acquire(&dirs.runtime, &waker, "ping") {
        single_instance::Outcome::Only(guard) => guard,
        _ => anyhow::bail!("Whats Pro is already running for this user"),
    };
    fastframe_log::Logging::new("zapfast", env!("CARGO_PKG_VERSION"))
        .filter(crate::diagnostics::default_log_filter(args.verbose))
        .panic_log(dirs.panic_log())
        .redact(crate::diagnostics::redact_protocol)
        .file(dirs.log_file())
        .init()?;
    log::info!("serving without a window");
    dirs.adopt_single_account()?;
    crate::bridge::start(&dirs.config, &dirs.state);
    let settings = crate::settings::Settings::load(&dirs.settings_file());
    let mut app = App::new(
        &waker,
        dirs,
        settings,
        AppOptions {
            tray: false,
            server: true,
        },
    )?;
    app.set_remote_control(&instance);
    eprintln!(
        "Whats Pro is serving. Log: {}",
        app.dirs.log_file().display()
    );
    let stop = Arc::new(AtomicBool::new(false));
    watch_signals(Arc::clone(&stop), waker.clone());
    let server = Server {
        app,
        stop,
        started: false,
        pair_phone: args.pair_phone,
        pair_asked: false,
        qr_png: args.qr_png,
        png_written: false,
        last_label: None,
        last_qr: None,
        last_code: None,
    };
    fastframe_shell::Shell::new(server, &waker)
        .start_hidden(true)
        .run(|_lease| {
            // `start_hidden` always agrees and nothing asks for a window.
            log::error!("serve was asked for a window");
            Ok::<(), std::convert::Infallible>(())
        })
        .unwrap_or_else(|never| match never {});
    drop(instance);
    log::info!("serve stopped");
    Ok(())
}

/// Ends the server cleanly on SIGTERM or Ctrl-C; a second one ends the
/// process at once.
fn watch_signals(stop: Arc<AtomicBool>, waker: Waker) {
    let spawned = std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    log::warn!("cannot watch for signals: {error}");
                    return;
                }
            };
            runtime.block_on(async {
                if !signalled().await {
                    return;
                }
                stop.store(true, Ordering::SeqCst);
                waker.wake();
                eprintln!("Whats Pro: stopping");
                if signalled().await {
                    std::process::exit(130);
                }
            });
        });
    if let Err(error) = spawned {
        log::warn!("cannot watch for signals: {error}");
    }
}

/// Waits for SIGTERM or SIGINT; `false` when they cannot be watched.
async fn signalled() -> bool {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut terminate), Ok(mut interrupt)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            log::warn!("cannot watch for signals");
            return false;
        };
        tokio::select! {
            _ = terminate.recv() => true,
            _ = interrupt.recv() => true,
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.is_ok()
}

/// The app held by the shell for the life of the server.
struct Server {
    app: App,
    stop: Arc<AtomicBool>,
    /// Whether the first tick started what needs the shell's context.
    started: bool,
    /// The phone number to link with a pairing code, and whether that code
    /// was asked for.
    pair_phone: Option<String>,
    pair_asked: bool,
    /// Where the QR code goes as a PNG, and whether one is there now.
    qr_png: Option<PathBuf>,
    png_written: bool,
    /// What was last reported, so each change is reported once. The QR code
    /// is remembered by a hash, never kept.
    last_label: Option<&'static str>,
    last_qr: Option<u64>,
    last_code: Option<String>,
}

impl Server {
    /// Tells the terminal how to link; the log gets the state alone.
    fn report_link(&mut self) {
        let link = self.app.link.clone();
        let label = link.log_label();
        // The worker logs each change as its label, which the log also
        // writes to stderr.
        if self.last_label != Some(label) {
            self.last_label = Some(label);
            if matches!(link, LinkStatus::Unlinked { .. })
                && self.pair_phone.is_none()
                && self.qr_png.is_none()
                && !std::io::stderr().is_terminal()
            {
                eprintln!(
                    "Whats Pro: not linked. Run in a terminal, or with --qr-png <path> or \
                     --pair-phone <number>, to link it."
                );
            }
        }
        match link {
            LinkStatus::Unlinked { qr, pair_code, .. } => {
                if let Some(phone) = &self.pair_phone {
                    if !std::mem::replace(&mut self.pair_asked, true) {
                        self.app.backend.send(Command::PairWithPhone(phone.clone()));
                        eprintln!("Whats Pro: asking WhatsApp for a pairing code");
                    }
                    if let Some(code) = pair_code
                        && self.last_code.as_ref() != Some(&code)
                    {
                        eprintln!(
                            "Pairing code: {code}\nOn the phone: Linked devices > Link a device > \
                             Link with phone number instead, then enter the code."
                        );
                        self.last_code = Some(code);
                    }
                } else if let Some(qr) = qr {
                    self.show_qr(&qr);
                }
            }
            LinkStatus::Connected | LinkStatus::Connecting => self.remove_png(),
            LinkStatus::LoggedOut => {
                self.pair_asked = false;
                self.last_qr = None;
                self.last_code = None;
            }
            _ => {}
        }
    }

    /// Shows a QR code once per code: on the terminal when stderr is one,
    /// and in the PNG when asked for.
    fn show_qr(&mut self, qr: &str) {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        qr.hash(&mut hasher);
        let hash = hasher.finish();
        if self.last_qr == Some(hash) {
            return;
        }
        self.last_qr = Some(hash);
        let terminal = std::io::stderr().is_terminal();
        if terminal && let Some(art) = crate::qr::to_terminal(qr) {
            eprintln!(
                "{art}\nScan with WhatsApp on the phone: Linked devices > Link a device. \
                 The code changes every few seconds."
            );
        }
        if let Some(path) = self.qr_png.clone() {
            match crate::qr::to_png(qr, 8).map(|png| write_private(&path, &png)) {
                Some(Ok(())) => {
                    self.png_written = true;
                    if !terminal {
                        eprintln!("Whats Pro: QR code saved to {}", path.display());
                    }
                }
                Some(Err(error)) => log::warn!("could not save the QR code: {error}"),
                None => log::warn!("could not draw the QR code"),
            }
        }
    }

    fn remove_png(&mut self) {
        if std::mem::take(&mut self.png_written)
            && let Some(path) = &self.qr_png
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Replaces `path` with `bytes`, readable by its owner only.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut staging = path.as_os_str().to_owned();
    staging.push(".saving");
    let staging = PathBuf::from(staging);
    let _ = std::fs::remove_file(&staging);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&staging)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&staging, path)
}

impl fastframe_shell::Resident for Server {
    fn closed(&self) -> fastframe_shell::Closed {
        fastframe_shell::Closed::Quit
    }

    fn window_gone(&mut self) {
        self.app.window_gone();
    }

    fn headless_frame(&mut self, ctx: &egui::Context) -> fastframe_shell::Headless {
        if !std::mem::replace(&mut self.started, true) {
            // What the window's first frame starts on the desktop: the MCP's
            // project changes repaint through this context, and the analysis
            // runs on its own schedule.
            crate::project_actions::set_ctx(ctx);
            crate::projects::engine::start(
                ctx,
                &self.app.dirs.config,
                &self.app.account().dirs.archive_db(),
            );
        }
        self.app.background_frame(ctx);
        self.report_link();
        // Another launch asking to be shown has no window to show here.
        self.app.wants_show = false;
        if self.app.quit_requested || self.stop.load(Ordering::SeqCst) {
            fastframe_shell::Headless::Quit
        } else {
            fastframe_shell::Headless::Wait
        }
    }

    fn start_hidden(&mut self) -> bool {
        self.app.start_hidden();
        true
    }

    fn shutdown(&mut self) {
        self.remove_png();
        self.app.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pairing_number_is_its_digits() {
        assert_eq!(phone_digits("+54 9 11 2233-4455").unwrap(), "5491122334455");
        assert_eq!(phone_digits("5491122334455").unwrap(), "5491122334455");
        assert!(phone_digits("+54").is_err());
        assert!(phone_digits("call me").is_err());
    }

    #[test]
    fn serve_takes_its_flags() {
        let args = Args::try_parse_from([
            "zapfast serve",
            "--secrets-dir",
            "/srv/secrets",
            "--pair-phone",
            "+5491122334455",
            "--qr-png",
            "/srv/qr.png",
            "--verbose",
        ])
        .unwrap();
        assert_eq!(args.secrets_dir, Some(PathBuf::from("/srv/secrets")));
        assert_eq!(args.pair_phone.as_deref(), Some("5491122334455"));
        assert_eq!(args.qr_png, Some(PathBuf::from("/srv/qr.png")));
        assert!(args.verbose);
    }

    #[cfg(unix)]
    #[test]
    fn the_qr_png_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("qr.png");
        write_private(&path, b"one").unwrap();
        write_private(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
