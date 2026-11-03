//! Dependencies: programs a task needs that are not in its process tree (WSL, Docker, a
//! database …). A dependency is a name: a known one stands for its background processes,
//! anything else is an executable name. Matched against the process list on every check.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::sys::ProcessEntry;

/// Known names and the processes behind them (lower case, `.exe` optional).
const KNOWN: &[(&str, &[&str])] = &[
    ("wsl", &["wslservice", "wslhost", "wslrelay", "vmmemwsl", "vmmem"]),
    (
        "docker",
        &["docker desktop", "com.docker.backend", "com.docker.build", "dockerd", "vpnkit", "vmmemwsl", "vmmem", "wslservice"],
    ),
];

/// A program in a task's tree that means the task depends on one of the known names.
const AUTO: &[(&str, &str)] = &[("wsl", "wsl"), ("wslhost", "wsl"), ("docker", "docker"), ("docker-compose", "docker")];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dep {
    pub name: String,
    /// Found in the task's tree rather than given by whoever protected it.
    #[serde(default)]
    pub auto: bool,
}

/// `Python.EXE` → `python`.
pub fn normalize(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    lower.strip_suffix(".exe").map(str::to_owned).unwrap_or(lower)
}

fn patterns(dep: &str) -> Vec<String> {
    let n = normalize(dep);
    match KNOWN.iter().find(|(k, _)| *k == n) {
        Some((_, procs)) => procs.iter().map(|p| (*p).to_owned()).collect(),
        None => vec![n],
    }
}

pub fn executable_names(deps: &[Dep]) -> BTreeSet<String> {
    deps.iter().flat_map(|d| patterns(&d.name)).collect()
}

/// Dependency name → running process ids.
pub fn resolve(deps: &[Dep], procs: &[ProcessEntry]) -> BTreeMap<String, Vec<u32>> {
    deps.iter()
        .map(|d| {
            let wanted = patterns(&d.name);
            let pids = procs.iter().filter(|p| wanted.contains(&normalize(&p.name))).map(|p| p.pid).collect();
            (d.name.clone(), pids)
        })
        .collect()
}

/// Known dependencies the tree's own programs point to.
pub fn detect<'a>(names: impl IntoIterator<Item = &'a str>) -> BTreeSet<&'static str> {
    names.into_iter().filter_map(|n| AUTO.iter().find(|(p, _)| *p == normalize(n)).map(|(_, d)| *d)).collect()
}

/// Adds `name` unless it is there already; true when added.
pub fn add(deps: &mut Vec<Dep>, name: &str, auto: bool) -> bool {
    let n = normalize(name);
    if n.is_empty() || deps.iter().any(|d| normalize(&d.name) == n) {
        return false;
    }
    deps.push(Dep { name: n, auto });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, name: &str) -> ProcessEntry {
        ProcessEntry { pid, parent_pid: 0, name: name.into() }
    }

    #[test]
    fn known_names_stand_for_their_processes_and_others_are_program_names() {
        let procs = [p(1, "wslservice.exe"), p(2, "vmmemWSL"), p(3, "postgres.exe"), p(4, "Postgres.exe"), p(5, "x.exe")];
        let deps = vec![Dep { name: "wsl".into(), auto: false }, Dep { name: "postgres.exe".into(), auto: false }];
        let r = resolve(&deps, &procs);
        assert_eq!(r["wsl"], [1, 2]);
        assert_eq!(r["postgres.exe"], [3, 4]);
    }

    #[test]
    fn wsl_and_docker_in_the_tree_are_detected() {
        assert_eq!(detect(["bash.exe", "WSL.exe", "docker.exe", "python.exe"]), BTreeSet::from(["wsl", "docker"]));
        let mut deps = vec![];
        assert!(add(&mut deps, "WSL", true));
        assert!(!add(&mut deps, "wsl.exe", false), "same name");
        assert!(!add(&mut deps, " ", false));
        assert_eq!(deps, [Dep { name: "wsl".into(), auto: true }]);
    }
}
