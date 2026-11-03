//! Following one protected process tree. Normally the OS does it (a job object: children
//! started later join by themselves, and totals include processes that have exited). When
//! that is refused (for example the root runs elevated), the tree is followed through parent
//! ids on every check instead, which misses grandchildren whose parent has already exited.
//!
//! Each process is identified by pid **and** start time, because pids are reused.

use std::collections::BTreeMap;

use crate::sys::{ProcessEntry, System, TreeTracker, Usage};

pub enum Tracking {
    Job(Box<dyn TreeTracker>),
    /// Why the OS could not track it.
    Walk(String),
}

pub struct Tree {
    /// pid → start time.
    pub members: BTreeMap<u32, i64>,
    pub tracking: Tracking,
    last_total: Usage,
    last_each: BTreeMap<u32, Usage>,
    pub memory_mb: u64,
}

/// What changed since the previous sample.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Sample {
    pub members_changed: bool,
    pub cpu_ms: u64,
    pub io_bytes: u64,
}

fn started_at(sys: &dyn System, pid: u32) -> Option<i64> {
    sys.process_info(pid).ok().map(|i| i.started_at)
}

/// `known` members that are still the same processes, plus every process started by one
/// of them (after it started), repeated until nothing new turns up.
pub fn descendants(sys: &dyn System, procs: &[ProcessEntry], known: &BTreeMap<u32, i64>) -> BTreeMap<u32, i64> {
    let running: BTreeMap<u32, &ProcessEntry> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut set: BTreeMap<u32, i64> = known
        .iter()
        .filter(|(pid, at)| running.contains_key(pid) && started_at(sys, **pid) == Some(**at))
        .map(|(p, a)| (*p, *a))
        .collect();
    loop {
        let mut added = false;
        for p in procs {
            if set.contains_key(&p.pid) {
                continue;
            }
            let Some(&parent_at) = set.get(&p.parent_pid) else { continue };
            if let Some(at) = started_at(sys, p.pid).filter(|at| *at >= parent_at) {
                set.insert(p.pid, at);
                added = true;
            }
        }
        if !added {
            return set;
        }
    }
}

fn delta(now: u64, before: u64) -> u64 {
    now.saturating_sub(before)
}

impl Tree {
    /// Starts following the still-running processes of `known` and everything they started,
    /// rooted at `root` if it still runs (else at any of them). No members: nothing runs.
    pub fn start(sys: &dyn System, root: u32, known: &BTreeMap<u32, i64>) -> Tree {
        let procs = sys.processes().unwrap_or_default();
        let members = descendants(sys, &procs, known);
        let root = if members.contains_key(&root) { Some(root) } else { members.keys().next().copied() };
        let tracking = match root {
            None => Tracking::Walk("nothing of the task runs any more".into()),
            Some(root) => {
                let others: Vec<u32> = members.keys().copied().filter(|p| *p != root).collect();
                match sys.track_tree(root, &others) {
                    Ok(t) => Tracking::Job(t),
                    Err(e) => Tracking::Walk(e.message),
                }
            }
        };
        let mut tree = Tree { members, tracking, last_total: Usage::default(), last_each: BTreeMap::new(), memory_mb: 0 };
        // The baseline: activity counts from here on.
        tree.sample(sys, &procs);
        tree
    }

    pub fn walking(&self) -> Option<&str> {
        match &self.tracking {
            Tracking::Walk(why) => Some(why),
            Tracking::Job(_) => None,
        }
    }

    pub fn sample(&mut self, sys: &dyn System, procs: &[ProcessEntry]) -> Sample {
        let before: Vec<u32> = self.members.keys().copied().collect();
        let mut s = Sample::default();
        match &self.tracking {
            Tracking::Job(job) => {
                if let Ok(pids) = job.members() {
                    let mut next = BTreeMap::new();
                    for pid in pids {
                        let at = self.members.get(&pid).copied().or_else(|| started_at(sys, pid));
                        if let Some(at) = at {
                            next.insert(pid, at);
                        }
                    }
                    self.members = next;
                }
                if let Ok(u) = job.usage() {
                    s.cpu_ms = delta(u.cpu_ms, self.last_total.cpu_ms);
                    s.io_bytes = delta(u.io_bytes, self.last_total.io_bytes);
                    self.memory_mb = u.memory_mb;
                    self.last_total = u;
                }
            }
            Tracking::Walk(_) => {
                self.members = descendants(sys, procs, &self.members);
                let mut each = BTreeMap::new();
                for pid in self.members.keys() {
                    if let Ok(u) = sys.process_usage(*pid) {
                        if let Some(old) = self.last_each.get(pid) {
                            s.cpu_ms += delta(u.cpu_ms, old.cpu_ms);
                            s.io_bytes += delta(u.io_bytes, old.io_bytes);
                        }
                        each.insert(*pid, u);
                    }
                }
                self.memory_mb = each.values().map(|u| u.memory_mb).sum();
                self.last_each = each;
            }
        }
        s.members_changed = self.members.keys().copied().ne(before);
        s
    }
}

/// Per-process usage of processes outside the tree (dependencies), between two samples.
#[derive(Default)]
pub struct Others {
    last: BTreeMap<u32, Usage>,
}

impl Others {
    pub fn sample(&mut self, sys: &dyn System, pids: &[u32]) -> Sample {
        let mut s = Sample::default();
        let mut next = BTreeMap::new();
        for pid in pids {
            if let Ok(u) = sys.process_usage(*pid) {
                if let Some(old) = self.last.get(pid) {
                    s.cpu_ms += delta(u.cpu_ms, old.cpu_ms);
                    s.io_bytes += delta(u.io_bytes, old.io_bytes);
                }
                next.insert(*pid, u);
            }
        }
        self.last = next;
        s
    }
}
