use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use serde::Serialize;
use tokio::sync::broadcast;

use crate::now_ms;
use crate::session::{Holder, Owner};

/// What happened to the share, as the plugin hears it on /events.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    Claimed { owner: Owner },
    Holder { holder: Holder, by: String },
    Revoked { agent_id: String, reason: String },
    /// The human's words to the agent, with the ids of the images sent with them. Text is cleared once acked.
    Message { text: String, attachments: Vec<String> },
    /// Messages dropped unread because the queue was full.
    Dropped { messages: u64 },
    /// One tool call, as it ran: shown in the viewer, not kept for resume.
    Action { agent_title: String, tool: String, summary: String, ok: bool, x: Option<i32>, y: Option<i32> },
    /// The PC behind a bridge became reachable (`down` null) or not, and why.
    Reach { down: Option<String> },
}

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    /// 0 for transient events (actions), which are never replayed.
    pub id: u64,
    pub at_ms: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

struct Ring {
    next: u64,
    events: VecDeque<Event>,
    attachments: HashMap<String, Vec<u8>>,
    attachment_order: VecDeque<String>,
    attachment_bytes: usize,
}

/// A bounded log of share events with live fan-out, resumable by event id. Human messages stay until acked;
/// images are held under a byte cap, oldest first out, and freed on ack.
pub struct Events {
    ring: Mutex<Ring>,
    cap: usize,
    attachment_cap: usize,
    tx: broadcast::Sender<Event>,
}

pub const ATTACHMENT_CAP: usize = 64 << 20;

fn is_message(kind: &EventKind) -> bool {
    matches!(kind, EventKind::Message { .. })
}

impl Events {
    pub fn new(cap: usize) -> Self {
        Self::with_attachment_cap(cap, ATTACHMENT_CAP)
    }

    pub fn with_attachment_cap(cap: usize, attachment_cap: usize) -> Self {
        let (tx, _) = broadcast::channel(256);
        Events {
            ring: Mutex::new(Ring { next: 1, events: VecDeque::new(), attachments: HashMap::new(), attachment_order: VecDeque::new(), attachment_bytes: 0 }),
            cap: cap.max(2),
            attachment_cap,
            tx,
        }
    }

    pub fn push(&self, kind: EventKind) -> u64 {
        let mut ring = self.ring.lock().unwrap();
        let id = ring.next;
        ring.next += 1;
        let event = Event { id, at_ms: now_ms(), kind };
        ring.events.push_back(event.clone());
        let mut dropped = 0u64;
        while ring.events.len() > self.cap {
            // Drop the oldest non-message first; messages go only when nothing else is left to drop.
            let at = ring.events.iter().position(|e| !is_message(&e.kind)).filter(|&i| i + 1 < ring.events.len()).unwrap_or(0);
            if let Some(gone) = ring.events.remove(at) {
                if let EventKind::Message { attachments, .. } = gone.kind {
                    dropped += 1;
                    for a in attachments {
                        Self::free(&mut ring, &a);
                    }
                }
            }
        }
        let _ = self.tx.send(event);
        if dropped > 0 {
            drop(ring);
            self.push(EventKind::Dropped { messages: dropped });
        }
        id
    }

    /// Shown live to whoever listens, never stored.
    pub fn transient(&self, kind: EventKind) {
        let _ = self.tx.send(Event { id: 0, at_ms: now_ms(), kind });
    }

    /// The human's message, with its images stored for /attachments until acked.
    pub fn message(&self, text: String, pngs: Vec<Vec<u8>>) -> u64 {
        let mut ids = Vec::new();
        let mut dropped = 0u64;
        {
            let mut ring = self.ring.lock().unwrap();
            for png in pngs {
                let id = format!("img-{}-{}", ring.next, ids.len() + 1);
                ring.attachment_bytes += png.len();
                ring.attachments.insert(id.clone(), png);
                ring.attachment_order.push_back(id.clone());
                ids.push(id);
            }
            while ring.attachment_bytes > self.attachment_cap {
                let Some(oldest) = ring.attachment_order.front().cloned() else { break };
                if ids.contains(&oldest) && ids.len() == ring.attachment_order.len() {
                    break;
                }
                Self::free(&mut ring, &oldest);
                dropped += 1;
            }
        }
        if dropped > 0 {
            log::warn!("dropped {dropped} unacked images over the cap");
        }
        self.push(EventKind::Message { text, attachments: ids })
    }

    fn free(ring: &mut Ring, id: &str) {
        if let Some(png) = ring.attachments.remove(id) {
            ring.attachment_bytes -= png.len();
        }
        ring.attachment_order.retain(|a| a != id);
    }

    pub fn attachment(&self, id: &str) -> Option<Vec<u8>> {
        self.ring.lock().unwrap().attachments.get(id).cloned()
    }

    /// Delivered: every message up to `upto` loses its text and images. Returns how many were cleared.
    pub fn ack(&self, upto: u64) -> usize {
        let mut ring = self.ring.lock().unwrap();
        let mut freed = Vec::new();
        let mut cleared = 0;
        for event in ring.events.iter_mut().filter(|e| e.id <= upto) {
            if let EventKind::Message { text, attachments } = &mut event.kind {
                if !text.is_empty() || !attachments.is_empty() {
                    cleared += 1;
                }
                text.clear();
                freed.append(attachments);
            }
        }
        for id in freed {
            Self::free(&mut ring, &id);
        }
        cleared
    }

    /// Stored events after `after`, oldest first, and whether some were lost in between.
    pub fn since_with_gap(&self, after: u64) -> (Vec<Event>, bool) {
        let ring = self.ring.lock().unwrap();
        let first = ring.events.front().map_or(ring.next, |e| e.id);
        let gap = after + 1 < first && after + 1 < ring.next;
        (ring.events.iter().filter(|e| e.id > after).cloned().collect(), gap && after > 0)
    }

    pub fn since(&self, after: u64) -> Vec<Event> {
        self.since_with_gap(after).0
    }

    pub fn last_id(&self) -> u64 {
        self.ring.lock().unwrap().next - 1
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// The backlog after `after` and a live feed, taken together so nothing falls between them.
    pub fn resume(&self, after: u64) -> (Vec<Event>, bool, broadcast::Receiver<Event>) {
        let rx = self.tx.subscribe();
        let (backlog, gap) = self.since_with_gap(after);
        (backlog, gap, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_returns_what_came_after() {
        let events = Events::new(16);
        let a = events.push(EventKind::Holder { holder: Holder::Human, by: "x".into() });
        let b = events.message("hi".into(), vec![vec![1, 2, 3]]);
        assert_eq!(events.since(a).iter().map(|e| e.id).collect::<Vec<_>>(), vec![b]);
        assert!(events.since(b).is_empty());
    }

    #[test]
    fn ack_clears_text_and_frees_images() {
        let events = Events::new(16);
        let id = events.message("look".into(), vec![vec![9; 10]]);
        let EventKind::Message { attachments, .. } = events.since(0)[0].kind.clone() else { panic!() };
        assert_eq!(events.attachment(&attachments[0]), Some(vec![9; 10]));
        assert_eq!(events.ack(id), 1);
        assert_eq!(events.attachment(&attachments[0]), None);
        assert!(matches!(&events.since(0)[0].kind, EventKind::Message { text, attachments } if text.is_empty() && attachments.is_empty()));
    }

    #[test]
    fn a_full_queue_drops_other_events_before_messages_and_says_so() {
        let events = Events::new(3);
        events.message("one".into(), vec![]);
        events.message("two".into(), vec![]);
        events.push(EventKind::Holder { holder: Holder::Human, by: "x".into() });
        events.push(EventKind::Holder { holder: Holder::Agent, by: "y".into() });
        let kept = events.since(0);
        assert_eq!(kept.iter().filter(|e| is_message(&e.kind)).count(), 2);
        events.message("three".into(), vec![]);
        events.message("four".into(), vec![]);
        let kept = events.since(0);
        assert!(kept.iter().any(|e| matches!(e.kind, EventKind::Dropped { .. })));
        assert!(kept.iter().any(|e| matches!(&e.kind, EventKind::Message { text, .. } if text == "four")));
        let (_, gap) = events.since_with_gap(1);
        assert!(gap);
    }

    #[test]
    fn images_over_the_cap_go_oldest_first() {
        let events = Events::with_attachment_cap(16, 10);
        events.message("a".into(), vec![vec![0; 6]]);
        events.message("b".into(), vec![vec![0; 6]]);
        let ids: Vec<String> = events
            .since(0)
            .into_iter()
            .filter_map(|e| if let EventKind::Message { attachments, .. } = e.kind { attachments.first().cloned() } else { None })
            .collect();
        assert_eq!(events.attachment(&ids[0]), None);
        assert!(events.attachment(&ids[1]).is_some());
    }
}
