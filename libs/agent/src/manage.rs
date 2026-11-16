//! The PC's management lane: commands from Rig that need no desktop session, as SYSTEM by default or as the signed-in
//! user, run at once or kept as background jobs. It is admitted by the PC's own agent-shell switch and by the human's
//! Block on the calling agent, never by who holds the desktop: a command touches no input, so the person at the PC is
//! not waited on and not interrupted. Every command, refused or run, goes to the journal and to /events.
//!
//! On disk, under the lane's directory (readable by SYSTEM and administrators alone, which the host sees to):
//!   runs/<id>/  one command at a time, removed once answered
//!   jobs/<id>/  a background job: `meta.json`, its output `o`, and `x` once it ended (its exit code, or why the PC ended it)

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    events::EventKind,
    journal::{Entry, Journal},
    now_ms,
    session::{Owner, Shared},
    shell::{self, Asked, Ended, Printed},
};

/// At most this many background jobs run at once on a PC.
pub const MAX_JOBS: usize = 8;
/// A background job is ended after this long, and its directory removed this long after it ended.
pub const JOB_LIMIT: Duration = Duration::from_secs(24 * 3600);
/// A background job whose output passes this is ended: its file is the PC's disk.
pub const JOB_OUTPUT_LIMIT: u64 = 16 << 20;
/// A command longer than this is refused at the boundary.
pub const COMMAND_MAX: usize = 32 * 1024;
/// How much of a command an event carries; the journal keeps all of it.
const EVENT_COMMAND: usize = 300;

/// Whom a command runs as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunAs {
    System,
    User,
}

/// What a SYSTEM run gave: how it ended, and the CPU time its process used where the host can tell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ran {
    pub ended: Ended,
    pub cpu_ms: Option<u64>,
}

/// How the host runs commands. Every call blocks.
pub trait Runner: Send + Sync + 'static {
    /// Whether this PC lets agents run commands: Err says why not, such as its agent-shell switch being off.
    fn allowed(&self) -> Result<(), String>;
    /// One command as SYSTEM, off every desktop, with its output and errors in `dir/o`; its whole process tree is ended
    /// after `limit`.
    fn run_system(&self, command: &str, dir: &Path, limit: Duration) -> Result<Ran, String>;
    /// One command as the signed-in user, as the desktop's own shell runs it; Err when nobody is signed in.
    fn run_user(&self, command: &str, limit: Duration) -> Result<(Printed, Ended), String>;
    /// Starts a command as SYSTEM, off every desktop, and returns its process id at once: its output goes to `dir/o`,
    /// and its exit code to `dir/x` when it ends.
    fn start_system(&self, command: &str, dir: &Path) -> Result<u32, String>;
    /// Whether the process `pid` this lane started still runs.
    fn alive(&self, pid: u32) -> bool;
    /// Ends the process `pid` and every process it started.
    fn end(&self, pid: u32);
    /// This boot of the PC: the same until it restarts.
    fn boot(&self) -> String;
}

/// Why the lane would not run a command; each is its own refusal.
#[derive(Clone, Debug, PartialEq)]
pub enum Refused {
    /// The PC's agent shell is off.
    Off(String),
    Blocked { since_ms: u64 },
    JobsFull,
    NoJob(String),
    Asked(String),
}

impl Refused {
    pub fn code(&self) -> &'static str {
        match self {
            Refused::Off(_) => "shell_off",
            Refused::Blocked { .. } => "blocked",
            Refused::JobsFull => "jobs_full",
            Refused::NoJob(_) => "no_job",
            Refused::Asked(_) => "bad_request",
        }
    }

    pub fn words(&self) -> String {
        match self {
            Refused::Off(why) => format!("this PC's agent shell is off: {why}"),
            Refused::Blocked { since_ms } => crate::session::Refusal::Blocked { since_ms: *since_ms }.words(),
            Refused::JobsFull => format!("this PC already runs {MAX_JOBS} background jobs, its most; read one until it ends, then start another"),
            Refused::NoJob(id) => format!("no background job {id} on this PC: it never ran here, or ended over a day ago and was removed"),
            Refused::Asked(why) => why.clone(),
        }
    }
}

/// One request to the lane, read once at the boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Run { asked: Asked, run_as: RunAs },
    Start { command: String, run_as: RunAs },
    Read { job: String },
}

/// The calling agent and what it asked, from POST /shell's body.
pub fn request(body: &Value) -> Result<(Owner, Request), String> {
    let agent = body.get("agent").ok_or("shell wants agent: {id, title, graph}")?;
    let owner = Owner {
        agent_id: agent.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).ok_or("shell wants agent.id")?.to_string(),
        title: agent.get("title").and_then(Value::as_str).unwrap_or("agent").to_string(),
        graph: agent.get("graph").and_then(Value::as_str).unwrap_or("").to_string(),
    };
    if let Some(job) = body.get("job").filter(|j| !j.is_null()) {
        let job = job.as_str().filter(|j| is_job_id(j)).ok_or("job is the id a background start answered")?;
        return Ok((owner, Request::Read { job: job.to_string() }));
    }
    let run_as = match body.get("as").and_then(Value::as_str) {
        None | Some("system") => RunAs::System,
        Some("user") => RunAs::User,
        Some(other) => return Err(format!("as is system or user, not {other}")),
    };
    let asked = shell::asked(body)?;
    if asked.command.len() > COMMAND_MAX {
        return Err(format!("the command is {} bytes, over the {COMMAND_MAX} this lane takes", asked.command.len()));
    }
    if body.get("background").and_then(Value::as_bool).unwrap_or(false) {
        if run_as == RunAs::User {
            return Err("background jobs run as SYSTEM only; run as the user without background".into());
        }
        return Ok((owner, Request::Start { command: asked.command, run_as }));
    }
    Ok((owner, Request::Run { asked, run_as }))
}

fn is_job_id(id: &str) -> bool {
    id.len() == 10 && id.starts_with("j-") && id[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// What the agent reads, and whether the command did what it asked.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Answer {
    pub ok: bool,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Meta {
    id: String,
    owner: Owner,
    run_as: RunAs,
    command: String,
    boot: String,
    pid: u32,
    started_ms: u64,
    /// Set once the end was journaled, so it is journaled once.
    #[serde(default)]
    told: bool,
}

/// How a background job stands, derived from its directory, the PC's boot and its process.
#[derive(Clone, Debug, PartialEq)]
enum JobState {
    Running,
    Exited(i32),
    /// The PC ended it: past its time limit, its output limit, or for a reboot.
    EndedBy(String),
}

fn job_state(meta: &Meta, x: Option<&str>, boot: &str, alive: impl FnOnce() -> bool) -> JobState {
    match x.map(str::trim) {
        Some(code) => match code.parse::<i32>() {
            Ok(code) => JobState::Exited(code),
            Err(_) => JobState::EndedBy(code.to_string()),
        },
        None if meta.boot != boot => JobState::EndedBy("the PC rebooted".into()),
        None if alive() => JobState::Running,
        None => JobState::EndedBy("its process ended without an exit code".into()),
    }
}

pub struct Lane {
    runner: Arc<dyn Runner>,
    dir: PathBuf,
    shared: Arc<Shared>,
    journal: Arc<Journal>,
    /// Held while a job starts, so two starts cannot both pass the cap.
    starting: Mutex<()>,
}

impl Lane {
    pub fn new(runner: Arc<dyn Runner>, dir: PathBuf, shared: Arc<Shared>, journal: Arc<Journal>) -> Arc<Self> {
        Arc::new(Lane { runner, dir, shared, journal, starting: Mutex::new(()) })
    }

    pub fn runner(&self) -> &Arc<dyn Runner> {
        &self.runner
    }

    fn admit(&self, owner: &Owner) -> Result<(), Refused> {
        self.runner.allowed().map_err(Refused::Off)?;
        match self.shared.blocked_since(&owner.agent_id) {
            Some(since_ms) => Err(Refused::Blocked { since_ms }),
            None => Ok(()),
        }
    }

    /// Admits and serves one request. Blocks for as long as a command runs.
    pub fn serve(&self, owner: &Owner, request: Request) -> Result<Answer, Refused> {
        if let Err(refused) = self.admit(owner) {
            let (command, run_as) = match &request {
                Request::Run { asked, run_as } => (asked.command.as_str(), Some(*run_as)),
                Request::Start { command, run_as } => (command.as_str(), Some(*run_as)),
                Request::Read { .. } => return Err(refused),
            };
            self.record(owner, run_as, command, &format!("refused: {}", refused.code()), json!({"reason": refused.words()}), None);
            return Err(refused);
        }
        match request {
            Request::Run { asked, run_as } => Ok(self.run(owner, &asked, run_as)),
            Request::Start { command, run_as } => self.start(owner, &command, run_as),
            Request::Read { job } => self.read(&job),
        }
    }

    fn record(&self, owner: &Owner, run_as: Option<RunAs>, command: &str, outcome: &str, mut detail: Value, job: Option<&str>) {
        if let Value::Object(map) = &mut detail {
            map.insert("graph".into(), json!(owner.graph));
            map.insert("as".into(), json!(run_as));
            map.insert("command".into(), json!(command));
            if let Some(job) = job {
                map.insert("job".into(), json!(job));
            }
        }
        self.journal.record(&Entry::new(owner.who(), "shell", outcome, detail.clone()));
        let took_ms = detail.get("took_ms").and_then(Value::as_u64);
        self.shared.events.push(EventKind::Shell {
            owner: owner.clone(),
            run_as,
            command: command.chars().take(EVENT_COMMAND).collect(),
            outcome: outcome.to_string(),
            took_ms,
            job: job.map(str::to_string),
        });
    }

    fn run(&self, owner: &Owner, asked: &Asked, run_as: RunAs) -> Answer {
        let began = Instant::now();
        let ran = match run_as {
            RunAs::User => self.runner.run_user(&asked.command, asked.timeout),
            RunAs::System => self.run_in_own_dir(&asked.command, asked.timeout),
        };
        let took_ms = began.elapsed().as_millis() as u64;
        match ran {
            Err(why) => {
                self.record(owner, Some(run_as), &asked.command, "failed", json!({"took_ms": took_ms, "error": why}), None);
                Answer { ok: false, text: format!("cannot run the command: {why}") }
            }
            Ok((printed, ended)) => {
                let (output, cut) = shell::output(&printed);
                let (outcome, exit_code) = match ended {
                    Ended::Exited(code) => (format!("exit {code}"), Some(code)),
                    Ended::TimedOut(_) => ("timed out".to_string(), None),
                };
                let detail = json!({"took_ms": took_ms, "exit_code": exit_code, "timed_out": exit_code.is_none(), "cut_bytes": cut, "output_bytes": output.len()});
                self.record(owner, Some(run_as), &asked.command, &outcome, detail, None);
                match shell::answer(&printed, ended) {
                    Ok(called) => Answer { ok: exit_code == Some(0), text: called.contents.iter().map(text_of).collect() },
                    Err(words) => Answer { ok: false, text: words },
                }
            }
        }
    }

    fn run_in_own_dir(&self, command: &str, limit: Duration) -> Result<(Printed, Ended), String> {
        let dir = self.dir.join("runs").join(format!("r-{:08x}", rand::random::<u32>()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
        let ran = self.runner.run_system(command, &dir, limit).and_then(|ran| {
            let printed = shell::printed_file(&dir.join("o")).map_err(|e| format!("cannot read the command's output: {e}"))?;
            Ok((printed, ran.ended))
        });
        let _ = std::fs::remove_dir_all(&dir);
        ran
    }

    fn jobs_dir(&self) -> PathBuf {
        self.dir.join("jobs")
    }

    fn metas(&self) -> Vec<(PathBuf, Meta)> {
        let Ok(entries) = std::fs::read_dir(self.jobs_dir()) else { return Vec::new() };
        entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let dir = e.path();
                let meta = std::fs::read(dir.join("meta.json")).ok().and_then(|b| serde_json::from_slice::<Meta>(&b).ok())?;
                Some((dir, meta))
            })
            .collect()
    }

    fn state_of(&self, dir: &Path, meta: &Meta) -> JobState {
        let x = std::fs::read_to_string(dir.join("x")).ok();
        job_state(meta, x.as_deref(), &self.runner.boot(), || self.runner.alive(meta.pid))
    }

    fn start(&self, owner: &Owner, command: &str, run_as: RunAs) -> Result<Answer, Refused> {
        let _starting = self.starting.lock().unwrap();
        let running = self.metas().iter().filter(|(dir, meta)| self.state_of(dir, meta) == JobState::Running).count();
        if running >= MAX_JOBS {
            self.record(owner, Some(run_as), command, "refused: jobs_full", json!({"reason": Refused::JobsFull.words()}), None);
            return Err(Refused::JobsFull);
        }
        let id = format!("j-{:08x}", rand::random::<u32>());
        let dir = self.jobs_dir().join(&id);
        let mut meta = Meta { id: id.clone(), owner: owner.clone(), run_as, command: command.to_string(), boot: self.runner.boot(), pid: 0, started_ms: now_ms(), told: false };
        let started = std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot make {}: {e}", dir.display()))
            .and_then(|_| self.runner.start_system(command, &dir))
            .and_then(|pid| {
                meta.pid = pid;
                write_meta(&dir, &meta)
            });
        if let Err(why) = started {
            let _ = std::fs::remove_dir_all(&dir);
            self.record(owner, Some(run_as), command, "failed", json!({"error": why}), None);
            return Ok(Answer { ok: false, text: format!("cannot start the job: {why}") });
        }
        self.record(owner, Some(run_as), command, "started", json!({"pid": meta.pid}), Some(&id));
        Ok(Answer { ok: true, text: json!({"job": id, "state": "running", "said": "read it with desk/shell(job=...) until it ends"}).to_string() })
    }

    fn read(&self, id: &str) -> Result<Answer, Refused> {
        let dir = self.jobs_dir().join(id);
        let meta = std::fs::read(dir.join("meta.json")).ok().and_then(|b| serde_json::from_slice::<Meta>(&b).ok()).ok_or_else(|| Refused::NoJob(id.to_string()))?;
        let state = self.state_of(&dir, &meta);
        let printed = shell::printed_file(&dir.join("o")).unwrap_or_default();
        let (output, cut) = shell::output(&printed);
        let ended_ms = std::fs::metadata(dir.join("x")).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64);
        let mut answer = json!({"job": id, "command": meta.command, "started_ms": meta.started_ms, "output": output, "cut_bytes": cut});
        let ok = match &state {
            JobState::Running => {
                answer["state"] = json!("running");
                true
            }
            JobState::Exited(code) => {
                answer["state"] = json!("exited");
                answer["exit_code"] = json!(code);
                answer["ended_ms"] = json!(ended_ms);
                *code == 0
            }
            JobState::EndedBy(why) => {
                answer["state"] = json!("ended");
                answer["why"] = json!(format!("ended because {why}"));
                answer["ended_ms"] = json!(ended_ms);
                false
            }
        };
        Ok(Answer { ok, text: answer.to_string() })
    }

    /// Ends jobs past their limits, marks those a reboot ended, journals each end once, and removes jobs a day after they
    /// ended. Run every few seconds; blocks on the disk and the host's process calls.
    pub fn sweep(&self) {
        for (dir, mut meta) in self.metas() {
            let mut state = self.state_of(&dir, &meta);
            if state == JobState::Running {
                let size = std::fs::metadata(dir.join("o")).map(|m| m.len()).unwrap_or(0);
                let over = if now_ms().saturating_sub(meta.started_ms) > JOB_LIMIT.as_millis() as u64 {
                    Some(format!("it ran past {} h", JOB_LIMIT.as_secs() / 3600))
                } else if size > JOB_OUTPUT_LIMIT {
                    Some(format!("its output passed {} MiB", JOB_OUTPUT_LIMIT >> 20))
                } else {
                    None
                };
                if let Some(why) = over {
                    let _ = std::fs::write(dir.join("x"), &why);
                    self.runner.end(meta.pid);
                    state = self.state_of(&dir, &meta);
                }
            }
            if let JobState::EndedBy(why) = &state {
                if !dir.join("x").exists() {
                    let _ = std::fs::write(dir.join("x"), why);
                }
            }
            if state == JobState::Running {
                continue;
            }
            if !meta.told {
                let took_ms = std::fs::metadata(dir.join("x")).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| (d.as_millis() as u64).saturating_sub(meta.started_ms));
                let (outcome, detail) = match &state {
                    JobState::Exited(code) => (format!("exit {code}"), json!({"exit_code": code, "took_ms": took_ms, "timed_out": false})),
                    JobState::EndedBy(why) => ("ended".to_string(), json!({"why": why, "took_ms": took_ms, "timed_out": why.contains("ran past")})),
                    JobState::Running => continue,
                };
                self.record(&meta.owner, Some(meta.run_as), &meta.command, &outcome, detail, Some(&meta.id));
                meta.told = true;
                if let Err(why) = write_meta(&dir, &meta) {
                    log::warn!("agent shell: job {} end not kept: {why}", meta.id);
                }
            }
            let ended_long_ago = std::fs::metadata(dir.join("x")).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map_or(false, |age| age > JOB_LIMIT);
            if ended_long_ago {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
}

fn write_meta(dir: &Path, meta: &Meta) -> Result<(), String> {
    let bytes = serde_json::to_vec(meta).map_err(|e| e.to_string())?;
    let tmp = dir.join("meta.json.new");
    std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, dir.join("meta.json"))).map_err(|e| format!("cannot keep the job's record: {e}"))
}

fn text_of(content: &crate::desk::Content) -> String {
    match content {
        crate::desk::Content::Text(text) => text.clone(),
        crate::desk::Content::Png(_) => String::new(),
    }
}

/// Sweeps the lane's jobs every few seconds for as long as the process runs.
pub async fn sweep_forever(lane: Arc<Lane>) {
    let mut every = tokio::time::interval(Duration::from_secs(5));
    loop {
        every.tick().await;
        let lane = lane.clone();
        let _ = tokio::task::spawn_blocking(move || lane.sweep()).await;
    }
}

/// The lane on a Linux host, through `sh` as whoever runs the process: what `fake_desk` and the tests stand on. There is
/// no other user to run as here, so `run_user` runs as this process too.
#[cfg(target_os = "linux")]
pub struct ShRunner {
    pub on: std::sync::atomic::AtomicBool,
    pub boot: Mutex<String>,
}

#[cfg(target_os = "linux")]
impl ShRunner {
    pub fn new() -> Self {
        ShRunner { on: std::sync::atomic::AtomicBool::new(true), boot: Mutex::new("boot-1".into()) }
    }

    fn sh(script: &str, dir: &Path) -> std::io::Result<std::process::Child> {
        use std::os::unix::process::CommandExt;
        std::fs::write(dir.join("c.sh"), script)?;
        std::process::Command::new("sh").arg("-c").arg("sh ./c.sh > o 2>&1").current_dir(dir).process_group(0).spawn()
    }
}

#[cfg(target_os = "linux")]
impl Runner for ShRunner {
    fn allowed(&self) -> Result<(), String> {
        if self.on.load(std::sync::atomic::Ordering::SeqCst) {
            Ok(())
        } else {
            Err("turned off by the install line".into())
        }
    }

    fn run_system(&self, command: &str, dir: &Path, limit: Duration) -> Result<Ran, String> {
        let mut child = Self::sh(command, dir).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return Ok(Ran { ended: Ended::Exited(status.code().unwrap_or(-1)), cpu_ms: None });
            }
            if Instant::now() >= deadline {
                self.end(child.id());
                let _ = child.wait();
                return Ok(Ran { ended: Ended::TimedOut(limit), cpu_ms: None });
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn run_user(&self, command: &str, limit: Duration) -> Result<(Printed, Ended), String> {
        let dir = std::env::temp_dir().join(format!("rigdesk-user-{:08x}", rand::random::<u32>()));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let ran = self.run_system(command, &dir, limit).and_then(|ran| Ok((shell::printed_file(&dir.join("o")).map_err(|e| e.to_string())?, ran.ended)));
        let _ = std::fs::remove_dir_all(&dir);
        ran
    }

    fn start_system(&self, command: &str, dir: &Path) -> Result<u32, String> {
        let script = format!("{command}\necho $? > x.new && mv x.new x\n");
        let mut child = Self::sh(&script, dir).map_err(|e| e.to_string())?;
        let pid = child.id();
        std::thread::spawn(move || child.wait());
        Ok(pid)
    }

    fn alive(&self, pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(false, |stat| stat.rsplit(')').next().map_or(false, |rest| !rest.trim_start().starts_with('Z')))
    }

    fn end(&self, pid: u32) {
        let _ = std::process::Command::new("kill").args(["-KILL", &format!("-{pid}")]).status();
    }

    fn boot(&self) -> String {
        self.boot.lock().unwrap().clone()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::events::Events;

    fn lane() -> (Arc<Lane>, Arc<ShRunner>, Arc<Shared>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("rigdesk-lane-{:08x}", rand::random::<u32>()));
        let runner = Arc::new(ShRunner::new());
        let shared = Arc::new(Shared::new(Events::new(256)));
        let lane = Lane::new(runner.clone(), dir.join("shell"), shared.clone(), Arc::new(Journal::open(dir.join("actions.jsonl"), 1 << 20)));
        (lane, runner, shared, dir)
    }

    fn asked(body: Value) -> (Owner, Request) {
        request(&body).unwrap()
    }

    #[test]
    fn a_command_needs_no_session_and_waits_for_no_human_but_the_switch_and_a_block_refuse_it() {
        let (lane, runner, shared, dir) = lane();
        let link = shared.mint();
        shared.claim(&link.grant, &link.secret, Owner { agent_id: "other".into(), title: "Other".into(), graph: "g".into() }).unwrap();
        assert!(shared.human_input("keyboard"));
        let (owner, run) = asked(json!({"agent": {"id": "a1", "title": "Ops", "graph": "fleet"}, "command": "echo hi; exit 3"}));
        let ran = lane.serve(&owner, run.clone()).unwrap();
        assert!(!ran.ok);
        assert!(ran.text.contains("\"exit_code\":3") && ran.text.contains("hi"), "{}", ran.text);

        runner.on.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(lane.serve(&owner, run.clone()).unwrap_err().code(), "shell_off");
        runner.on.store(true, std::sync::atomic::Ordering::SeqCst);
        let link = shared.mint();
        shared.claim(&link.grant, &link.secret, owner.clone()).unwrap();
        assert!(shared.block());
        shared.revoke("the agent left");
        assert_eq!(lane.serve(&owner, run).unwrap_err().code(), "blocked", "a block outlives the session");

        let journal = Journal::open(dir.join("actions.jsonl"), 1 << 20).tail(10);
        let shells: Vec<&Entry> = journal.iter().filter(|e| e.what == "shell").collect();
        assert_eq!(shells.iter().map(|e| e.outcome.as_str()).collect::<Vec<_>>(), vec!["exit 3", "refused: shell_off", "refused: blocked"]);
        assert_eq!(shells[0].detail["graph"], json!("fleet"));
        assert_eq!(shells[0].detail["as"], json!("system"));
        assert_eq!(shells[0].detail["command"], json!("echo hi; exit 3"));
        assert!(shells[0].detail["took_ms"].is_u64());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_job_outlives_its_call_is_capped_and_a_reboot_ends_it() {
        let (lane, runner, _shared, dir) = lane();
        let owner = Owner { agent_id: "a1".into(), title: "Ops".into(), graph: "fleet".into() };
        let start = |command: &str| lane.serve(&owner, Request::Start { command: command.into(), run_as: RunAs::System });
        let quick: Value = serde_json::from_str(&start("echo done").unwrap().text).unwrap();
        let mut ids = Vec::new();
        for _ in 0..MAX_JOBS - 1 {
            let started: Value = serde_json::from_str(&start("sleep 30").unwrap().text).unwrap();
            ids.push(started["job"].as_str().unwrap().to_string());
        }
        let read = |id: &str| -> Value { serde_json::from_str(&lane.serve(&owner, Request::Read { job: id.into() }).unwrap().text).unwrap() };
        let deadline = Instant::now() + Duration::from_secs(5);
        while read(quick["job"].as_str().unwrap())["state"] != json!("exited") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let done = read(quick["job"].as_str().unwrap());
        assert_eq!((done["exit_code"].clone(), done["output"].as_str().unwrap().trim()), (json!(0), "done"));
        start("sleep 30").unwrap();
        assert_eq!(start("sleep 30").unwrap_err().code(), "jobs_full");

        for id in &ids {
            let pid = serde_json::from_slice::<Meta>(&std::fs::read(dir.join("shell/jobs").join(id).join("meta.json")).unwrap()).unwrap().pid;
            runner.end(pid);
        }
        *runner.boot.lock().unwrap() = "boot-2".into();
        assert_eq!(read(&ids[0])["why"], json!("ended because the PC rebooted"));
        lane.sweep();
        let ends: Vec<Entry> = Journal::open(dir.join("actions.jsonl"), 1 << 20).tail(64).into_iter().filter(|e| e.outcome == "ended").collect();
        assert_eq!(ends.len(), MAX_JOBS, "each ended job is journaled once: {ends:?}");
        lane.sweep();
        assert_eq!(Journal::open(dir.join("actions.jsonl"), 1 << 20).tail(64).iter().filter(|e| e.outcome == "ended").count(), MAX_JOBS);
        assert_eq!(lane.serve(&owner, Request::Read { job: "j-00000000".into() }).unwrap_err().code(), "no_job");
        let _ = std::fs::remove_dir_all(dir);
    }
}
