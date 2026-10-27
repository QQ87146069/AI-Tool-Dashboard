//! Read-only observations of persisted local turns, cross-checked with their live owner.
//! Logs have finite retention: this is a lower bound, never a global idle assertion.
use super::tool_home;
use crate::model::{Activity, Reading};
use chrono::DateTime;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const MAX_SCAN: u64 = 64 * 1024 * 1024;
const BLOCK: u64 = 64 * 1024;
const MAX_RECORD: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub(super) struct CodexActivity {
    home: Option<PathBuf>,
    watermark: i64,
    owners: HashMap<String, Owner>,
    tails: HashMap<PathBuf, Tail>,
}

struct Owner {
    pid: Pid,
    logged_at: u64,
}

struct Tail {
    size: u64,
    offset: u64,
    modified: SystemTime,
    event: Option<Event>,
}

#[derive(Clone, Debug, PartialEq)]
struct Event {
    started: bool,
    at: u64,
}

#[derive(Deserialize)]
struct Envelope {
    timestamp: String,
    #[serde(rename = "type")]
    kind: String,
    payload: Payload,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
}

// Only deserialize lifecycle fields. Messages, prompts and tool outputs are ignored.
fn event(line: &[u8]) -> Option<Event> {
    let envelope: Envelope = serde_json::from_slice(line).ok()?;
    if envelope.kind != "event_msg" {
        return None;
    }
    let started = match envelope.payload.kind.as_deref()? {
        "task_started" | "turn_started" => true,
        "task_complete" | "turn_complete" | "turn_aborted" => false,
        _ => return None,
    };
    let at = DateTime::parse_from_rfc3339(&envelope.timestamp)
        .ok()?
        .timestamp()
        .try_into()
        .ok()?;
    Some(Event { started, at })
}

fn latest(bytes: &[u8]) -> std::io::Result<Option<Event>> {
    for line in bytes.rsplit(|b| *b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let envelope: Envelope = serde_json::from_slice(line)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
        if envelope.kind == "event_msg"
            && matches!(
                envelope.payload.kind.as_deref(),
                Some(
                    "task_started"
                        | "turn_started"
                        | "task_complete"
                        | "turn_complete"
                        | "turn_aborted"
                )
            )
        {
            return event(line)
                .map(Some)
                .ok_or_else(|| std::io::ErrorKind::InvalidData.into());
        }
    }
    Ok(None)
}

fn owner(process: &str, logged_at: u64) -> Option<Owner> {
    let mut parts = process.split(':');
    if parts.next()? != "pid" {
        return None;
    }
    let pid = parts.next()?.parse::<u32>().ok().filter(|id| *id > 0)?;
    let uuid = parts.next()?;
    if parts.next().is_some()
        || uuid.len() != 36
        || !uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    {
        return None;
    }
    Some(Owner {
        pid: Pid::from_u32(pid),
        logged_at,
    })
}

fn readonly(path: &Path) -> rusqlite::Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_millis(100))?;
    db.pragma_update(None, "query_only", true)?;
    Ok(db)
}

fn read_tail(path: &Path, previous: Option<&Tail>) -> std::io::Result<Tail> {
    let mut file = File::open(path)?;
    let before = file.metadata()?;
    let size = before.len();
    let mut offset = size;
    let mut suffix = Vec::new();
    let mut found = None;
    let mut read_offset = size;
    let mut boundary = false;
    if let Some(previous) = previous
        && size > previous.size
        && size - previous.offset <= MAX_RECORD as u64
    {
        file.seek(SeekFrom::Start(previous.offset))?;
        let mut bytes = vec![0; (size - previous.offset) as usize];
        file.read_exact(&mut bytes)?;
        if let Some(last) = bytes.iter().rposition(|b| *b == b'\n') {
            found = latest(&bytes[..last])?.or_else(|| previous.event.clone());
            read_offset = previous.offset + last as u64 + 1;
        } else {
            found = previous.event.clone();
            read_offset = previous.offset;
        }
        offset = 0;
    }
    while offset > 0 && size - offset < MAX_SCAN {
        let start = offset.saturating_sub(BLOCK);
        let mut bytes = vec![0; (offset - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut bytes)?;
        bytes.extend_from_slice(&suffix);
        if boundary {
            bytes.push(b'\n');
        }
        // Ignore both an incomplete leading record and a not-yet-flushed trailing record.
        let first = if start == 0 {
            Some(0)
        } else {
            bytes.iter().position(|b| *b == b'\n').map(|i| i + 1)
        };
        if let (Some(first), Some(last)) = (first, bytes.iter().rposition(|b| *b == b'\n')) {
            if !boundary {
                read_offset = start + last as u64 + 1;
            }
            boundary = true;
            found = if first <= last {
                latest(&bytes[first..last])?
            } else {
                None
            };
            if found.is_some() {
                break;
            }
            // Retain only the leading partial record; completed records need no rescan.
            suffix = bytes[..first.saturating_sub(1)].to_vec();
        } else {
            // No newline yet: this is part of the unfinished trailing record.
            suffix.clear();
        }
        if suffix.len() > MAX_RECORD {
            break;
        }
        offset = start;
    }
    let after = file.metadata()?;
    if size != after.len() || before.modified()? != after.modified()? {
        return Err(std::io::ErrorKind::WouldBlock.into());
    }
    Ok(Tail {
        size,
        offset: read_offset,
        modified: after.modified()?,
        event: found,
    })
}

fn live_turn(event: &Event, owner: &Owner, process_birth: u64, now: u64) -> bool {
    event.started
        && process_birth > 0
        && process_birth <= event.at
        && event.at <= owner.logged_at
        && owner.logged_at <= now
}

impl CodexActivity {
    pub(super) fn probe(&mut self) -> Reading<Activity> {
        let Some(home) = tool_home("CODEX_HOME", ".codex") else {
            return Reading::Unknown;
        };
        if self.home.as_ref() != Some(&home) {
            *self = Self {
                home: Some(home.clone()),
                ..Self::default()
            };
        }
        match self.count(&home) {
            Ok(running) if running > 0 => Reading::Observed {
                value: Activity {
                    running,
                    waiting: None,
                    lower_bound: true,
                },
                at: SystemTime::now(),
                valid_for: Duration::from_secs(15),
            },
            Ok(_) => Reading::Unknown,
            Err(_) => Reading::Failed,
        }
    }

    fn count(&mut self, home: &Path) -> Result<u32, Box<dyn std::error::Error>> {
        let logs = readonly(&home.join("logs_2.sqlite"))?;
        let newest: i64 =
            logs.query_row("SELECT COALESCE(MAX(id),0) FROM logs", [], |r| r.get(0))?;
        if newest < self.watermark {
            self.owners.clear();
            self.watermark = 0;
        }
        let query = if self.watermark == 0 {
            "SELECT thread_id, process_uuid, ts FROM logs WHERE id IN \
             (SELECT MAX(id) FROM logs WHERE thread_id IS NOT NULL GROUP BY thread_id)"
        } else {
            "SELECT thread_id, process_uuid, ts FROM logs \
             WHERE id > ?1 AND id <= ?2 AND thread_id IS NOT NULL ORDER BY id"
        };
        let mut stmt = logs.prepare(query)?;
        let mut rows = if self.watermark == 0 {
            stmt.query([])?
        } else {
            stmt.query([self.watermark, newest])?
        };
        while let Some(row) = rows.next()? {
            let thread: String = row.get(0)?;
            let process: String = row.get(1)?;
            let logged_at: i64 = row.get(2)?;
            if let Some(owner) = u64::try_from(logged_at)
                .ok()
                .and_then(|at| owner(&process, at))
            {
                self.owners.insert(thread, owner);
            } else {
                self.owners.remove(&thread);
            }
        }
        self.watermark = newest;
        let mut pids: Vec<_> = self.owners.values().map(|o| o.pid).collect();
        pids.sort_unstable();
        pids.dedup();
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
        );
        let state = readonly(&home.join("state_5.sqlite"))?;
        let mut path_query = state.prepare("SELECT rollout_path FROM threads WHERE id=?1")?;
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs();
        let mut running = 0;
        for (thread, owner) in &self.owners {
            let Some(process) = system.process(owner.pid) else {
                continue;
            };
            if !process.exists()
                || process
                    .exe()
                    .and_then(Path::file_stem)
                    .is_none_or(|name| !name.eq_ignore_ascii_case("codex"))
            {
                continue;
            }
            let Some(path): Option<PathBuf> = path_query
                .query_row([thread], |r| r.get::<_, String>(0).map(PathBuf::from))
                .optional()?
            else {
                continue;
            };
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let Ok(metadata) = path.metadata() else {
                continue;
            };
            let modified = metadata.modified()?;
            let changed = self
                .tails
                .get(&path)
                .is_none_or(|tail| tail.size != metadata.len() || tail.modified != modified);
            if changed {
                // A racing/truncated/unreadable file cannot retain an earlier active observation.
                let tail = read_tail(&path, self.tails.get(&path));
                self.tails.remove(&path);
                if let Ok(tail) = tail {
                    self.tails.insert(path.clone(), tail);
                }
            }
            if self
                .tails
                .get(&path)
                .and_then(|tail| tail.event.as_ref())
                .is_some_and(|e| live_turn(e, owner, process.start_time(), now))
            {
                running += 1;
            }
        }
        Ok(running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_lifecycle_events_are_activity_evidence() {
        let line = |kind| {
            format!(
                r#"{{"timestamp":"2026-10-26T00:00:00Z","type":"event_msg","payload":{{"type":"{kind}"}}}}"#
            )
        };
        assert!(event(line("task_started").as_bytes()).unwrap().started);
        for kind in ["task_complete", "turn_complete", "turn_aborted"] {
            assert!(!event(line(kind).as_bytes()).unwrap().started);
        }
        for kind in [
            "token_count",
            "agent_message",
            "user_message",
            "item_completed",
        ] {
            assert_eq!(event(line(kind).as_bytes()), None);
        }
        assert_eq!(event(b"{"), None);
    }

    #[test]
    fn dead_reused_or_unconfirmed_owners_do_not_validate_a_turn() {
        let owner = owner("pid:123:00000000-0000-0000-0000-000000000000", 110).unwrap();
        let start = Event {
            started: true,
            at: 100,
        };
        assert!(live_turn(&start, &owner, 90, 120));
        assert!(!live_turn(&start, &owner, 101, 120));
        assert!(!live_turn(&start, &owner, 0, 120));
        assert!(!live_turn(&start, &owner, 90, 109));
        assert!(!live_turn(
            &Event {
                started: false,
                at: 100
            },
            &owner,
            90,
            120
        ));
        assert!(!live_turn(
            &Event {
                started: true,
                at: 111
            },
            &owner,
            90,
            120
        ));
        assert!(super::owner("uuid-without-pid", 110).is_none());
    }

    #[test]
    fn tail_finds_events_across_blocks_and_ignores_partial_writes() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("ai-dashboard-tail-{}.jsonl", std::process::id()));
        let start = b"{\"timestamp\":\"2026-10-26T00:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n";
        let mut bytes = start.to_vec();
        bytes.extend(vec![b' '; BLOCK as usize + 150]);
        bytes.extend(b"\n{\"timestamp\":");
        std::fs::write(&path, &bytes).unwrap();
        let tail = read_tail(&path, None).unwrap();
        assert!(tail.event.as_ref().unwrap().started);
        bytes.extend(b"\"2026-10-26T00:01:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n");
        std::fs::write(&path, bytes).unwrap();
        assert!(
            !read_tail(&path, Some(&tail))
                .unwrap()
                .event
                .unwrap()
                .started
        );
        assert!(!read_tail(&path, None).unwrap().event.unwrap().started);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn corrupt_records_cannot_preserve_an_active_observation() {
        assert!(latest(b"{broken}\n").is_err());
        assert!(
            latest(
                br#"{"timestamp":"invalid","type":"event_msg","payload":{"type":"task_complete"}}"#
            )
            .is_err()
        );
        assert_eq!(latest(b"\n  \n").unwrap(), None);
    }
}
