//! `rigdesk-bridge`: one stock RustDesk PC, between a Rig agent and the people watching it.
//!
//! The PC runs unmodified RustDesk with "Enable direct IP access" on, reachable over the tailnet. This process logs in to
//! it as a RustDesk client with the PC's permanent password and holds the one session to it. It serves RigDesk's agent
//! API on loopback, so the Rig plugin treats it as one more PC, and, with a viewer port, relays that same session to
//! RigDesk GUIs, which dial the bridge instead of the PC. The connection opens when the agent first acts or a GUI
//! attaches, and closes after a quiet spell with no GUI, so the PC shows a RustDesk session only while it is in use.
//!
//!   rigdesk-bridge --peer <tailnet name or 100.x address[:port]> [--password-file <file>] --token-file <file>
//!   rigdesk-bridge --id <RustDesk ID> --id-server <hbbs host[:port]> --key-file <hbbs public key file> ...
//!                  [--port 21120] [--journal <file>] [--name "Rig agent"] [--once] [--login-id <ID, with --peer>]
//!                  [--viewer-port 21128 --viewer-password-file <file>]
//!                  [--mcp-server http://<host>[:port][/path] [--mcp-token-file <file>]]...
//!                  [--invite-file <file> --pair-key-file <file>, with --once, --peer and --login-id]
//!   rigdesk-bridge --pair-key <file>
//!
//! `--invite-file` redeems an invite the device made, with Rig's pairing key, in place of a password (see `invite`);
//! `--pair-key` makes that key if the file has none and prints its public half, which the device keeps.
//! The environment's `RIGDESK_PAIR_KEY_FILE` names that key for proving a knock to a device that asks for it (see `knock`).
//! It is not an argument so that a Rig newer than this binary can set it for an older one, which ignores it.
//! With no `--password-file`, only a connection the person at the PC allows on Rig's `POST /offer` carries a session (see
//! `knock`); with one, such a connection carries the session the same way, and the password serves unattended access.
//! Each `--mcp-server` names an MCP server on the PC's own address (a phone's native tools, say), offered to the holding
//! agent as a found server; the `--mcp-token-file` after it holds the bearer that server wants.
//! The passwords and the API bearer are read from files (0600) so they never sit on a command line.

mod desk;
mod invite;
mod keys;
mod knock;
mod rendezvous;
mod link;
mod relay;
mod terminal;

use std::path::PathBuf;
use std::sync::Arc;

use hbb_common::{log, tokio};
use rigdesk_agent::found::Remote;
use rigdesk_agent::{Api, Events, Found, Journal, Shared};

const JOURNAL_BYTES: u64 = 8 * 1024 * 1024;
const EVENT_RING: usize = 512;
const PAIR_KEY_ENV: &str = "RIGDESK_PAIR_KEY_FILE";

struct Args {
    route: (link::Route, String),
    password_file: Option<PathBuf>,
    token_file: PathBuf,
    port: u16,
    journal: Option<PathBuf>,
    name: String,
    viewer: Option<(u16, PathBuf)>,
    servers: Vec<Remote>,
    once: bool,
    invite: Option<(PathBuf, PathBuf)>,
}

fn parse_args() -> Result<Args, String> {
    let mut peer = None;
    let mut id = None;
    let mut id_server = None;
    let mut key_file = None;
    let mut password_file = None;
    let mut token_file = None;
    let mut port = 21120;
    let mut journal = None;
    let mut name = "Rig agent".to_string();
    let mut viewer_port = None;
    let mut viewer_password_file = None;
    let mut servers: Vec<(String, Option<PathBuf>)> = Vec::new();
    let mut once = false;
    let mut login_id = None;
    let mut invite_file = None;
    let mut pair_key_file = None;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--peer" => peer = Some(value()?),
            "--id" => id = Some(value()?),
            "--id-server" => id_server = Some(value()?),
            "--key-file" => key_file = Some(PathBuf::from(value()?)),
            "--password-file" => password_file = Some(PathBuf::from(value()?)),
            "--token-file" => token_file = Some(PathBuf::from(value()?)),
            "--port" => port = value()?.parse().map_err(|_| "--port needs a number".to_string())?,
            "--journal" => journal = Some(PathBuf::from(value()?)),
            "--name" => name = value()?,
            "--viewer-port" => viewer_port = Some(value()?.parse().map_err(|_| "--viewer-port needs a number".to_string())?),
            "--viewer-password-file" => viewer_password_file = Some(PathBuf::from(value()?)),
            "--mcp-server" => servers.push((value()?, None)),
            "--once" => once = true,
            "--login-id" => login_id = Some(value()?),
            "--invite-file" => invite_file = Some(PathBuf::from(value()?)),
            "--pair-key-file" => pair_key_file = Some(PathBuf::from(value()?)),
            "--mcp-token-file" => match servers.last_mut() {
                Some((_, file @ None)) => *file = Some(PathBuf::from(value()?)),
                _ => return Err("--mcp-token-file follows the --mcp-server it is for, once".into()),
            },
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let viewer = match (viewer_port, viewer_password_file) {
        (Some(port), Some(file)) => Some((port, file)),
        (None, None) => None,
        _ => return Err("--viewer-port and --viewer-password-file go together".into()),
    };
    let invite = match (invite_file, pair_key_file) {
        (Some(file), Some(key)) if once && password_file.is_none() && peer.is_some() && login_id.is_some() => Some((file, key)),
        (None, None) => None,
        _ => return Err("--invite-file and --pair-key-file go together, with --once, --peer and --login-id and no --password-file".into()),
    };
    if login_id.is_some() && peer.is_none() {
        return Err("--login-id goes with --peer".into());
    }
    let route = match (peer, id, id_server, key_file) {
        // A RigDesk PC registers nowhere, but refuses a login under any ID but its own: it is found by the ID it shows.
        (Some(peer), None, None, None) => match login_id {
            Some(id) => (link::Route::direct(&peer).0, rendezvous::parse_id(&id)?),
            None => link::Route::direct(&peer),
        },
        (None, Some(id), Some(host), Some(key_file)) => {
            let key = read_secret(&key_file, "ID server key")?;
            link::Route::by_id(rendezvous::parse_id(&id)?, rendezvous::Server::parse(&host, &key)?)
        }
        (None, None, None, None) => return Err("--peer, or --id with --id-server and --key-file, is required".into()),
        (Some(_), _, _, _) => return Err("--peer goes alone; --id, --id-server and --key-file are the other way".into()),
        _ => return Err("--id, --id-server and --key-file go together".into()),
    };
    let servers = servers
        .into_iter()
        .map(|(url, file)| Remote::parse(&url, file.map(|f| read_secret(&f, "MCP server token")).transpose()?))
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Args {
        route,
        password_file,
        token_file: token_file.ok_or("--token-file is required")?,
        port,
        journal,
        name,
        viewer,
        servers,
        once,
        invite,
    })
}

fn knock_key_file(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.filter(|file| !file.is_empty()).map(PathBuf::from)
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
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let [flag, file] = argv.as_slice() {
        if flag == "--pair-key" {
            match invite::pair_key(std::path::Path::new(file)) {
                Ok(public) => println!("{public}"),
                Err(e) => {
                    eprintln!("rigdesk-bridge: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
    }
    let mut args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("rigdesk-bridge: {e}");
            std::process::exit(2);
        }
    };
    let invite = match &args.invite {
        Some((file, key)) => match invite::Invite::load(file, key) {
            Ok(invite) => Some(invite),
            Err(e) => {
                eprintln!("rigdesk-bridge: {e}");
                std::process::exit(2);
            }
        },
        None => None,
    };
    if let Some(invite) = &invite {
        args.name = invite.display_name();
    }
    let password = args.password_file.as_ref().map_or(Ok(String::new()), |file| read_secret(file, "password"));
    let (password, bearer) = match (password, read_secret(&args.token_file, "token")) {
        (Ok(p), Ok(t)) => (p, t),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("rigdesk-bridge: {e}");
            std::process::exit(2);
        }
    };
    let viewer = match &args.viewer {
        Some((port, file)) => match read_secret(file, "viewer password") {
            Ok(p) => Some((*port, p)),
            Err(e) => {
                eprintln!("rigdesk-bridge: {e}");
                std::process::exit(2);
            }
        },
        None => None,
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("rigdesk-bridge: no runtime: {e}");
            std::process::exit(1);
        }
    };
    runtime.block_on(async move {
        let what = args.route.0.label();
        let shared = Arc::new(Shared::new(Events::new(EVENT_RING)));
        let link = link::Link::new(args.route, password, args.name.clone(), shared.clone());
        if args.once {
            link.set_once();
        }
        if let Some(invite) = invite {
            link.set_invite(invite);
        }
        tokio::spawn(link.clone().run());
        let journal = match &args.journal {
            Some(path) => Journal::open(path.clone(), JOURNAL_BYTES),
            None => Journal::memory(),
        };
        if let Some((port, password)) = viewer {
            match relay::Relay::new(link.clone(), shared.clone(), password).listen(port).await {
                Ok(port) => log::info!("rigdesk-bridge: viewers of {what} on 127.0.0.1:{port}"),
                Err(e) => {
                    eprintln!("rigdesk-bridge: cannot listen for viewers on 127.0.0.1:{port}: {e}");
                    std::process::exit(1);
                }
            }
        }
        let found = (!args.servers.is_empty()).then(|| Found::remote(args.servers));
        let pair_key = knock_key_file(std::env::var_os(PAIR_KEY_ENV));
        if let Some(key_file) = pair_key.clone() {
            link.set_pair_key(key_file);
        }
        let knocker = knock::Knocker::new(link.clone(), shared.clone(), tokio::runtime::Handle::current(), pair_key);
        let api = Api::hosted(shared, Arc::new(desk::RemoteDesk::new(link)), Arc::new(journal), &bearer, found);
        api.operator_joins(knocker);
        match api.serve(args.port).await {
            Ok(port) => {
                log::info!("rigdesk-bridge: agent API for {what} on 127.0.0.1:{port}");
                println!("listening {port}");
            }
            Err(e) => {
                eprintln!("rigdesk-bridge: cannot listen on 127.0.0.1:{}: {e}", args.port);
                std::process::exit(1);
            }
        }
        std::future::pending::<()>().await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_knock_key_comes_from_a_set_variable_only() {
        assert_eq!(knock_key_file(None), None);
        assert_eq!(knock_key_file(Some("".into())), None);
        assert_eq!(knock_key_file(Some("/keys/pair.key".into())), Some(PathBuf::from("/keys/pair.key")));
    }
}
