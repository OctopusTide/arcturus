//! Read-only survey of this machine's transcripts through the adapters: how many files each
//! agent has, how many were read to the end, and why the rest stopped. Prints counts only, no
//! conversation text.
//!
//! cargo run --release -p agents --example survey

use std::collections::BTreeMap;
use std::time::Instant;

use agents::model::{ReadOptions, Record, Stop};
use agents::{Env, all};

fn main() {
    let env = Env::from_process();
    for adapter in all() {
        let info = adapter.info();
        let started = Instant::now();
        let files = match adapter.transcripts(&env) {
            Ok(f) => f,
            Err(e) => {
                println!("{}: cannot list transcripts: {e}", info.id);
                continue;
            }
        };
        let bytes: u64 = files.iter().map(|f| f.size).sum();
        let mut stops: BTreeMap<String, u64> = BTreeMap::new();
        let mut ignored: BTreeMap<String, u64> = BTreeMap::new();
        let mut records: BTreeMap<&'static str, u64> = BTreeMap::new();
        let (mut done, mut bad_lines, mut read_bytes) = (0u64, 0u64, 0u64);
        let mut conversations = std::collections::BTreeSet::new();
        for f in &files {
            conversations.insert(f.conversation_id.clone());
            let out = adapter.read(f, 0, ReadOptions::default(), &mut |r| {
                let k = match r {
                    Record::Session { .. } => "session",
                    Record::Message { .. } => "message",
                    Record::ToolCall { .. } => "tool_call",
                    Record::ToolResult { .. } => "tool_result",
                    Record::FileWrite { .. } => "file_write",
                    Record::Command { .. } => "command",
                    Record::TurnFailed { .. } => "turn_failed",
                };
                *records.entry(k).or_default() += 1;
            });
            match out {
                Err(e) => *stops.entry(format!("io error: {}", e.kind())).or_default() += 1,
                Ok(o) => {
                    bad_lines += o.bad_lines;
                    read_bytes += o.offset;
                    for (k, v) in o.ignored {
                        *ignored.entry(k).or_default() += v;
                    }
                    match o.stop {
                        None => done += 1,
                        Some(Stop::Compressed) => *stops.entry("compressed (.zst)".to_owned()).or_default() += 1,
                        Some(Stop::UnknownVersion { version, .. }) => {
                            *stops.entry(format!("unknown version {version}")).or_default() += 1
                        }
                        Some(Stop::UnknownFormat { detail, .. }) => {
                            *stops.entry(format!("unknown format: {detail}")).or_default() += 1
                        }
                    }
                }
            }
        }
        println!(
            "== {} ({} files, {} conversations, {:.1} GB; read {:.1} GB in {:.1} s)",
            info.id,
            files.len(),
            conversations.len(),
            bytes as f64 / 1e9,
            read_bytes as f64 / 1e9,
            started.elapsed().as_secs_f64()
        );
        println!("   read to the end: {done}; bad lines skipped: {bad_lines}");
        for (k, v) in &stops {
            println!("   stopped: {v:>6}  {k}");
        }
        for (k, v) in &records {
            println!("   records: {v:>9}  {k}");
        }
        let mut ign: Vec<_> = ignored.into_iter().collect();
        ign.sort_by(|a, b| b.1.cmp(&a.1));
        for (k, v) in ign.iter().take(12) {
            println!("   ignored: {v:>9}  {k}");
        }
    }
}
