//! The PC's own health samples: every 10 minutes a script the host gives runs on the management lane as SYSTEM and prints
//! one JSON object of raw figures, read here once into a typed [`Figures`]. The PC keeps the last day of samples on disk,
//! serves them by id at GET /samples, and says each new one on /events. It reports figures only: whether they are healthy
//! is Rig's to decide.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{events::EventKind, manage::Lane, now_ms, session::Shared};

pub const SCHEMA: u32 = 1;
pub const INTERVAL: Duration = Duration::from_secs(600);
/// The first sample after the process starts, unless one was taken less than an interval ago.
pub const FIRST: Duration = Duration::from_secs(30);
/// A day of samples at the interval.
pub const KEPT: usize = 144;
const SCRIPT_LIMIT: Duration = Duration::from_secs(480);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Os {
    pub caption: String,
    pub version: String,
    /// The build with its update revision, as 26100.2314.
    pub build: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cpu {
    #[serde(default)]
    pub name: String,
    pub load_pct: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Ram {
    pub total_mb: u64,
    pub free_mb: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Volume {
    /// The drive, as C:.
    pub name: String,
    #[serde(default)]
    pub label: String,
    pub total_mb: u64,
    pub free_mb: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Updates {
    /// When Windows Update was last asked; the search is slow, so the PC asks hourly and repeats the answer between.
    pub checked_ms: Option<u64>,
    pub last_install_ms: Option<u64>,
    pub pending: Option<u32>,
    /// Why the search gave no answer; a time-out is unknown, never a fault.
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Defender {
    pub realtime_on: Option<bool>,
    pub signature_age_days: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Encryption {
    pub volume: String,
    /// On, Off or Unknown, as BitLocker says it.
    pub protection: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Disk {
    pub name: String,
    #[serde(default)]
    pub media: String,
    /// Healthy, Warning or Unhealthy, as Windows' storage stack says it.
    pub health: String,
    pub wear_pct: Option<u32>,
    pub temperature_c: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Service {
    pub name: String,
    #[serde(default)]
    pub display: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Battery {
    pub charge_pct: Option<u32>,
    #[serde(default)]
    pub status: String,
}

/// What the host's script printed, read once here. A probe that failed is missing and named in `errors`; a figure
/// missing for another reason (no battery, no BitLocker on this edition) is simply absent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Figures {
    pub schema: u32,
    pub os: Os,
    pub uptime_s: u64,
    pub cpu: Cpu,
    pub ram: Ram,
    #[serde(default)]
    pub volumes: Vec<Volume>,
    pub pending_reboot: Option<bool>,
    /// Which of Windows' pending-reboot markers are set.
    #[serde(default)]
    pub pending_reboot_why: Vec<String>,
    pub updates: Option<Updates>,
    pub defender: Option<Defender>,
    #[serde(default)]
    pub bitlocker: Vec<Encryption>,
    #[serde(default)]
    pub disks: Vec<Disk>,
    /// Services set to start automatically that are not running.
    #[serde(default)]
    pub stopped_services: Vec<Service>,
    pub system_errors_24h: Option<u32>,
    pub battery: Option<Battery>,
    #[serde(default)]
    pub errors: Vec<String>,
}

/// One run of the collector: the figures, or why there are none.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "taken", rename_all = "snake_case")]
pub enum Taken {
    Ok { figures: Figures },
    Failed { why: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Rises with each sample on this PC, across restarts and reboots: Rig reads those after the last it has.
    pub id: u64,
    pub at_ms: u64,
    pub boot: String,
    pub rigdesk: String,
    pub took_ms: u64,
    pub cpu_ms: Option<u64>,
    #[serde(flatten)]
    pub taken: Taken,
}

/// The figures in a script's output: its last line that is a JSON object.
pub fn figures(output: &str) -> Result<Figures, String> {
    let line = output.lines().rev().map(str::trim).find(|l| l.starts_with('{')).ok_or_else(|| format!("the script printed no JSON object: {}", head(output)))?;
    let figures: Figures = serde_json::from_str(line).map_err(|e| format!("the script's figures do not read: {e}"))?;
    if figures.schema != SCHEMA {
        return Err(format!("the script's figures are schema {}, this RigDesk reads {SCHEMA}", figures.schema));
    }
    Ok(figures)
}

fn head(text: &str) -> String {
    text.chars().take(400).collect()
}

/// The last day of samples, in memory and in a JSONL file rewritten past its bound.
pub struct Samples {
    path: Option<PathBuf>,
    kept: Mutex<VecDeque<Sample>>,
}

impl Samples {
    pub fn open(path: Option<PathBuf>) -> Self {
        let mut kept = VecDeque::new();
        if let Some(file) = path.as_ref().and_then(|p| std::fs::File::open(p).ok()) {
            for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
                match serde_json::from_str::<Sample>(&line) {
                    Ok(sample) => kept.push_back(sample),
                    Err(e) => log::warn!("health: a kept sample does not read and is dropped: {e}"),
                }
            }
        }
        while kept.len() > KEPT {
            kept.pop_front();
        }
        Samples { path, kept: Mutex::new(kept) }
    }

    pub fn after(&self, id: u64) -> Vec<Sample> {
        self.kept.lock().unwrap().iter().filter(|s| s.id > id).cloned().collect()
    }

    pub fn last(&self) -> Option<Sample> {
        self.kept.lock().unwrap().back().cloned()
    }

    /// The next sample's id: past the last kept, and its time where the clock allows.
    pub fn next_id(&self, at_ms: u64) -> u64 {
        self.last().map_or(at_ms, |last| at_ms.max(last.id + 1))
    }

    pub fn add(&self, sample: Sample) {
        let mut kept = self.kept.lock().unwrap();
        kept.push_back(sample.clone());
        let rewrite = kept.len() > KEPT;
        while kept.len() > KEPT {
            kept.pop_front();
        }
        let Some(path) = &self.path else { return };
        let written = if rewrite {
            let tmp = path.with_extension("jsonl.new");
            let mut text = String::new();
            for s in kept.iter() {
                text.push_str(&serde_json::to_string(s).unwrap_or_default());
                text.push('\n');
            }
            std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path))
        } else {
            std::fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| {
                let mut line = serde_json::to_string(&sample).unwrap_or_default();
                line.push('\n');
                f.write_all(line.as_bytes())
            })
        };
        if let Err(e) = written {
            log::error!("health: sample {} not kept on disk: {e}", sample.id);
        }
    }
}

/// Takes the samples: the host's script, run on the lane as SYSTEM in its own directory.
pub struct Collector {
    pub lane: Arc<Lane>,
    pub shared: Arc<Shared>,
    pub samples: Arc<Samples>,
    pub script: &'static str,
    /// The directory the script runs in, kept between runs so it can keep its own slow answers there.
    pub dir: PathBuf,
    pub rigdesk: String,
    pub interval: Duration,
}

impl Collector {
    /// One sample, kept and said on /events. Blocks while the script runs.
    pub fn take(&self) -> Sample {
        let began = Instant::now();
        let at_ms = now_ms();
        let ran = std::fs::create_dir_all(&self.dir).map_err(|e| format!("cannot make {}: {e}", self.dir.display())).and_then(|_| {
            let _ = std::fs::remove_file(self.dir.join("o"));
            self.lane.runner().run_system(self.script, &self.dir, SCRIPT_LIMIT)
        });
        let (taken, cpu_ms) = match ran {
            Err(why) => (Taken::Failed { why: format!("the script did not run: {why}") }, None),
            Ok(ran) => {
                let printed = crate::shell::printed_file(&self.dir.join("o")).unwrap_or_default();
                let output = crate::shell::plain(&[printed.head, printed.tail].concat());
                let taken = match ran.ended {
                    crate::shell::Ended::TimedOut(after) => Taken::Failed { why: format!("the script ran past {} s and was ended", after.as_secs()) },
                    crate::shell::Ended::Exited(_) => match figures(&output) {
                        Ok(figures) => Taken::Ok { figures },
                        Err(why) => Taken::Failed { why },
                    },
                };
                (taken, ran.cpu_ms)
            }
        };
        let sample = Sample { id: self.samples.next_id(at_ms), at_ms, boot: self.lane.runner().boot(), rigdesk: self.rigdesk.clone(), took_ms: began.elapsed().as_millis() as u64, cpu_ms, taken };
        if let Taken::Failed { why } = &sample.taken {
            log::warn!("health: sample {} failed: {why}", sample.id);
        }
        self.samples.add(sample.clone());
        self.shared.events.push(EventKind::Health { sample: sample.id });
        sample
    }

    /// The wait before the first sample: soon after the process starts, unless the last kept is younger than an interval.
    pub fn first_wait(&self) -> Duration {
        let soon = FIRST.min(self.interval);
        let since_last = self.samples.last().map(|s| Duration::from_millis(now_ms().saturating_sub(s.at_ms)));
        match since_last {
            Some(age) if age < self.interval => (self.interval - age).max(soon),
            _ => soon,
        }
    }

    pub async fn run_forever(self: Arc<Self>) {
        tokio::time::sleep(self.first_wait()).await;
        loop {
            let collector = self.clone();
            let began = tokio::time::Instant::now();
            let _ = tokio::task::spawn_blocking(move || collector.take()).await;
            tokio::time::sleep_until(began + self.interval).await;
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::{events::Events, journal::Journal, manage::ShRunner};

    const WINDOWS: &str = include_str!("../tests/fixtures/figures-windows.json");

    fn collector(dir: &std::path::Path, script: &'static str) -> Collector {
        let shared = Arc::new(Shared::new(Events::new(64)));
        let lane = Lane::new(Arc::new(ShRunner::new()), dir.join("shell"), shared.clone(), Arc::new(Journal::memory()));
        Collector { lane, shared, samples: Arc::new(Samples::open(Some(dir.join("health.jsonl")))), script, dir: dir.join("health"), rigdesk: "1.4.5-rig".into(), interval: INTERVAL }
    }

    #[test]
    fn a_windows_sample_reads_into_figures_and_a_broken_one_says_why() {
        let figures = figures(&format!("WARNING: a probe was slow\n{WINDOWS}\n")).unwrap();
        assert_eq!(figures.volumes[1].free_mb, 90000);
        assert_eq!(figures.updates.unwrap().pending, Some(2));
        assert!(super::figures("no json here").unwrap_err().contains("no JSON object"));
        assert!(super::figures(&WINDOWS.replace("\"schema\":1", "\"schema\":9")).unwrap_err().contains("schema 9"));
    }

    #[test]
    fn samples_are_kept_on_disk_a_day_long_and_read_after_an_id() {
        let dir = std::env::temp_dir().join(format!("rigdesk-health-{:08x}", rand::random::<u32>()));
        std::fs::create_dir_all(&dir).unwrap();
        let script: &'static str = Box::leak(format!("cat <<'J'\n{WINDOWS}\nJ\n").into_boxed_str());
        let taking = collector(&dir, script);
        let mut events = taking.shared.events.subscribe();
        let first = taking.take();
        assert!(matches!(first.taken, Taken::Ok { .. }), "{first:?}");
        assert!(matches!(events.try_recv().unwrap().kind, EventKind::Health { sample } if sample == first.id));
        assert!(taking.first_wait() > Duration::from_secs(500), "a sample just taken is not taken again at a restart");
        let failing = collector(&dir, "echo broken; exit 1");
        let second = failing.take();
        assert!(second.id > first.id);
        assert!(matches!(&second.taken, Taken::Failed { why } if why.contains("no JSON object")), "{second:?}");

        let reopened = Samples::open(Some(dir.join("health.jsonl")));
        assert_eq!(reopened.after(0).iter().map(|s| s.id).collect::<Vec<_>>(), vec![first.id, second.id]);
        assert_eq!(reopened.after(first.id).len(), 1);
        for _ in 0..KEPT {
            let at = now_ms();
            reopened.add(Sample { id: reopened.next_id(at), at_ms: at, ..second.clone() });
        }
        assert_eq!(Samples::open(Some(dir.join("health.jsonl"))).after(0).len(), KEPT);
        let _ = std::fs::remove_dir_all(dir);
    }
}
