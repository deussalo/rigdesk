//! `rigdesk-remote`: one stock RustDesk PC, driven for a Rig agent from the Rig host.
//!
//! The PC runs unmodified RustDesk with "Enable direct IP access" on, reachable over the tailnet. This process logs in to
//! it as a RustDesk client with the PC's permanent password, and serves RigDesk's agent API on loopback, so the Rig
//! plugin treats it as one more PC. The connection opens when the agent first acts and closes after a quiet spell, so
//! the PC shows a RustDesk session only while it is in use.
//!
//!   rigdesk-remote --peer <tailnet name or 100.x address[:port]> --password-file <file> --token-file <file>
//!                  [--port 21120] [--journal <file>] [--name "Rig agent"]
//!
//! The password and the API bearer are read from files (0600) so they never sit on a command line.

mod desk;
mod keys;
mod link;

use std::path::PathBuf;
use std::sync::Arc;

use hbb_common::{log, tokio};
use rigdesk_agent::{Api, Events, Journal, Shared};

const JOURNAL_BYTES: u64 = 8 * 1024 * 1024;
const EVENT_RING: usize = 512;

struct Args {
    peer: String,
    password_file: PathBuf,
    token_file: PathBuf,
    port: u16,
    journal: Option<PathBuf>,
    name: String,
}

fn parse_args() -> Result<Args, String> {
    let mut peer = None;
    let mut password_file = None;
    let mut token_file = None;
    let mut port = 21120;
    let mut journal = None;
    let mut name = "Rig agent".to_string();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--peer" => peer = Some(value()?),
            "--password-file" => password_file = Some(PathBuf::from(value()?)),
            "--token-file" => token_file = Some(PathBuf::from(value()?)),
            "--port" => port = value()?.parse().map_err(|_| "--port needs a number".to_string())?,
            "--journal" => journal = Some(PathBuf::from(value()?)),
            "--name" => name = value()?,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        peer: peer.ok_or("--peer is required")?,
        password_file: password_file.ok_or("--password-file is required")?,
        token_file: token_file.ok_or("--token-file is required")?,
        port,
        journal,
        name,
    })
}

fn read_secret(path: &PathBuf, what: &str) -> Result<String, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("cannot read the {what} file {}: {e}", path.display()))?;
    let s = s.trim().to_string();
    if s.is_empty() {
        return Err(format!("the {what} file {} is empty", path.display()));
    }
    Ok(s)
}

fn main() {
    hbb_common::env_logger::Builder::from_env(hbb_common::env_logger::Env::default().default_filter_or("info")).init();
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("rigdesk-remote: {e}");
            std::process::exit(2);
        }
    };
    let (password, bearer) = match (read_secret(&args.password_file, "password"), read_secret(&args.token_file, "token")) {
        (Ok(p), Ok(t)) => (p, t),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("rigdesk-remote: {e}");
            std::process::exit(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("rigdesk-remote: no runtime: {e}");
            std::process::exit(1);
        }
    };
    runtime.block_on(async move {
        let link = link::Link::new(args.peer.clone(), password, args.name.clone());
        tokio::spawn(link.clone().run());
        let journal = match &args.journal {
            Some(path) => Journal::open(path.clone(), JOURNAL_BYTES),
            None => Journal::memory(),
        };
        let shared = Arc::new(Shared::new(Events::new(EVENT_RING)));
        let api = Api::hosted(shared, Arc::new(desk::RemoteDesk::new(link)), Arc::new(journal), &bearer);
        match api.serve(args.port).await {
            Ok(port) => {
                log::info!("rigdesk-remote: agent API for {} on 127.0.0.1:{port}", args.peer);
                println!("listening {port}");
            }
            Err(e) => {
                eprintln!("rigdesk-remote: cannot listen on 127.0.0.1:{}: {e}", args.port);
                std::process::exit(1);
            }
        }
        std::future::pending::<()>().await;
    });
}
