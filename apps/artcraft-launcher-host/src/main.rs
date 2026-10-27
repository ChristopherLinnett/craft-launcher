//! ArtCraft Launcher's hosted mode.
//!
//! One server for a team: ArtCraft Launcher's browser build at the root of an address, and each
//! Craft App's browser build at `/<app>/`, all behind one shared password. The launcher in the
//! browser installs, updates and removes the apps on the server through `api/`
//! ([`artcraft_launcher_core::hosted`]); everyone who opens an app runs it in their own browser.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod auth;
mod host;
mod serve;
mod store;
mod web;

use std::io::BufRead;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage:
  artcraft-launcher-host serve --data <dir> --launcher <dir> [--listen 127.0.0.1:8120]
      Serve the launcher's web build from --launcher, and the apps it installs from --data.
  artcraft-launcher-host set-password --data <dir>
      Read the team password from the first line of stdin. Ends every signed-in session.";

fn main() -> ExitCode {
    logger::init();
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("artcraft-launcher-host: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let Some((command, rest)) = args.split_first() else { return Err(USAGE.into()) };
    let (mut data, mut launcher, mut listen) = (None, None, "127.0.0.1:8120".to_string());
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        let value = it.next().cloned().ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?;
        match flag.as_str() {
            "--data" => data = Some(PathBuf::from(value)),
            "--launcher" => launcher = Some(PathBuf::from(value)),
            "--listen" => listen = value,
            _ => return Err(format!("unknown option {flag}\n{USAGE}")),
        }
    }
    let data = data.ok_or_else(|| format!("--data is required\n{USAGE}"))?;
    match command.as_str() {
        "serve" => {
            let launcher = launcher.ok_or_else(|| format!("--launcher is required\n{USAGE}"))?;
            serve::run(&listen, host::Host::open(data, launcher)?)
        }
        "set-password" => {
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).map_err(|e| format!("couldn't read the password ({e})"))?;
            auth::Auth::set_password(&data, line.trim_end_matches(['\r', '\n']))?;
            println!("Password set. Everyone signed in has to sign in again.");
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

/// Log lines to stderr (the service manager's journal). `ARTCRAFT_HOST_LOG` sets the level.
mod logger {
    use log::{LevelFilter, Log, Metadata, Record};

    struct Stderr;
    static LOGGER: Stderr = Stderr;

    impl Log for Stderr {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.level() <= log::max_level()
        }

        fn log(&self, record: &Record<'_>) {
            if self.enabled(record.metadata()) {
                eprintln!("{} {}: {}", record.level(), record.target(), record.args());
            }
        }

        fn flush(&self) {}
    }

    pub fn init() {
        let level = std::env::var("ARTCRAFT_HOST_LOG").ok().and_then(|l| l.parse().ok()).unwrap_or(LevelFilter::Info);
        if log::set_logger(&LOGGER).is_ok() {
            log::set_max_level(level);
        }
    }
}
