//! A Linux PC's management lane alone, as RigDesk's root service serves it, for proving it off a desktop:
//!
//!   sudo cargo run -p rigdesk-agent --example lane_host -- <socket> <token file> <dir>
//!
//! It serves `/health`, `/log`, `/events` and `/shell` on the unix socket, connectable by root alone, for the bearer in
//! the token file. Commands run through systemd units, as root or as the user signed in at seat0. `<dir>` holds the switch
//! (`agent-shell`, root's, `on` or `off`), the journal (`actions.jsonl`) and the jobs (`shell/`); the person's pause is the
//! file `<dir>/paused`. With `LANE_HOST_USER` set, a command as the user runs as that user (who needs a systemd user
//! manager: `loginctl enable-linger`) instead of whoever is signed in at seat0. It prints `listening` once it serves.

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    use std::sync::Arc;

    use rigdesk_agent::{manage::Lane, systemd::{self, SystemdRunner}, Api, Events, Journal, Shared};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let [socket, token, dir] = args.as_slice() else {
        eprintln!("usage: lane_host <socket> <token file> <dir>");
        std::process::exit(2);
    };
    let dir = std::path::PathBuf::from(dir);
    let bearer = std::fs::read_to_string(token).expect("the token file").trim().to_string();
    let paused = dir.join("paused");
    let user: Box<dyn Fn() -> Result<systemd::SignedIn, String> + Send + Sync> = match std::env::var("LANE_HOST_USER") {
        Ok(name) => Box::new(move || systemd::user_named(&name)),
        Err(_) => Box::new(systemd::signed_in),
    };
    let runner = Arc::new(SystemdRunner { switch: dir.join("agent-shell"), paused: Box::new(move || paused.exists()), user });
    let shared = Arc::new(Shared::new(Events::new(256)));
    let journal = Arc::new(Journal::open(dir.join("actions.jsonl"), 1 << 20));
    let lane = Lane::new(runner, dir.join("shell"), shared.clone(), journal.clone());
    Api::lane_only(lane, shared, journal, &bearer).serve_unix(std::path::Path::new(socket)).await.expect("the socket");
    println!("listening");
    std::future::pending::<()>().await;
}

#[cfg(not(target_os = "linux"))]
fn main() {}
