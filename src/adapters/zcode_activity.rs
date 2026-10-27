//! Local built-in ZCode turns. Read lifecycle metadata only, never model-io rollouts.
use super::tool_home;
use crate::model::{Activity, Reading};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const BUDGET: u64 = 8 * 1024 * 1024;
const MAX_LINE: usize = 1024 * 1024;
const HEALTH_MS: i64 = 120_000;

#[derive(Default)]
pub(super) struct ZCodeActivity {
    home: Option<PathBuf>,
    last_probe: Option<Instant>,
    cursors: HashMap<PathBuf, Cursor>,
    owners: HashMap<u32, Owner>,
    sessions: HashMap<String, Session>,
    latest_cli_at: i64,
}

#[derive(Default)]
struct Cursor {
    offset: u64,
    prefix: Option<([u8; 32], usize)>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Owner {
    pid: u32,
    workspace_key: String,
    command: PathBuf,
    #[serde(skip)]
    host_pid: u32,
    #[serde(skip)]
    at: i64,
}

struct Runtime {
    pid: u32,
    parent: u32,
    born: i64,
    exe: PathBuf,
}

#[derive(Default)]
struct Session {
    workspace: Option<(i64, String)>,
    turn: Option<Turn>,
}

struct Turn {
    id: Option<String>,
    at: i64,
    active: bool,
    root: bool,
    last_seen: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    timestamp: String,
    event: Option<String>,
    module: String,
    session_id: Option<String>,
    turn_id: Option<String>,
    #[serde(default)]
    context: Context,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Context {
    workspace_key: Option<String>,
    parent_session_id: Option<String>,
}

fn owner(line: &[u8]) -> Option<Owner> {
    let line = std::str::from_utf8(line).ok()?;
    let (prefix, json) = line.split_once(" [zcode-agent] ZCode agent process started ")?;
    let (prefix, host_pid) = prefix.rsplit_once(" [pid:")?;
    let host_pid = host_pid.strip_suffix(']')?.parse().ok()?;
    let stamp = prefix.rsplit_once('[')?.1.strip_suffix(']')?;
    let stamp = NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M:%S%.f").ok()?;
    let at = Local
        .from_local_datetime(&stamp)
        .single()?
        .timestamp_millis();
    let mut owner: Owner = serde_json::from_str(json).ok()?;
    if owner.pid == 0 || owner.workspace_key.is_empty() || !owner.command.is_absolute() {
        return None;
    }
    owner.at = at;
    owner.host_pid = host_pid;
    Some(owner)
}

fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .replace('/', "\\")
            .eq_ignore_ascii_case(&right.to_string_lossy().replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn valid_owner(owner: &Owner, runtime: &Runtime, now: i64) -> bool {
    owner.pid == runtime.pid
        && owner.host_pid == runtime.parent
        && runtime.born > 0
        && runtime.born <= owner.at
        && owner.at - runtime.born <= 10_000
        && owner.at <= now
        && same_path(&owner.command, &runtime.exe)
}

fn runtimes() -> System {
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let candidates: Vec<_> = system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            let name = process.name().to_string_lossy().to_ascii_lowercase();
            (name.starts_with("zcode") || matches!(name.as_str(), "node" | "node.exe" | "nodejs"))
                .then_some(*pid)
        })
        .collect();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&candidates),
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );
    system
}

fn runtime_metadata(system: &System) -> Vec<Runtime> {
    system
        .processes()
        .values()
        .filter_map(|process| {
            let args = process.cmd();
            if !args.iter().any(|a| a == "app-server")
                || !args.iter().any(|a| a == "--stdio")
                || !args
                    .iter()
                    .any(|a| Path::new(a).file_name().is_some_and(|n| n == "zcode.cjs"))
            {
                return None;
            }
            Some(Runtime {
                pid: process.pid().as_u32(),
                parent: process.parent()?.as_u32(),
                born: i64::try_from(process.start_time())
                    .ok()?
                    .checked_mul(1000)?,
                exe: process.exe()?.into(),
            })
        })
        .collect()
}

fn daily_files(dir: &Path, prefix: &str, suffix: &str) -> io::Result<Vec<(NaiveDate, PathBuf)>> {
    let today = Local::now().date_naive();
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(date) = name
            .to_str()
            .and_then(|n| n.strip_prefix(prefix))
            .and_then(|n| n.strip_suffix(suffix))
            .and_then(|n| NaiveDate::parse_from_str(n, "%Y-%m-%d").ok())
        else {
            continue;
        };
        if date <= today && today.signed_duration_since(date).num_days() < 7 {
            files.push((date, entry.path()));
        }
    }
    files.sort_unstable_by_key(|file| std::cmp::Reverse(file.0));
    Ok(files)
}

// Complete lines only. A bounded prefix digest detects replacement/truncation without
// retaining raw log contents. Partial writes and catch-up never validate old counts.
fn scan(
    path: &Path,
    cursor: &mut Cursor,
    budget: &mut u64,
    mut apply: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    if size < cursor.offset {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let length = cursor.prefix.map_or(128.min(size as usize), |(_, len)| len);
    let mut prefix = vec![0; length];
    file.read_exact(&mut prefix)?;
    let digest: [u8; 32] = Sha256::digest(&prefix).into();
    if cursor.prefix.is_some_and(|(old, _)| old != digest) {
        return Err(io::ErrorKind::Interrupted.into());
    }
    if length > 0 {
        cursor.prefix = Some((digest, length));
    }
    file.seek(SeekFrom::Start(cursor.offset))?;
    let mut reader = BufReader::new(file.take((size - cursor.offset).min(*budget)));
    let mut line = Vec::new();
    loop {
        line.clear();
        let bytes = reader.read_until(b'\n', &mut line)?;
        *budget = budget.saturating_sub(bytes as u64);
        if bytes == 0 {
            break;
        }
        if bytes > MAX_LINE {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if line.last() != Some(&b'\n') {
            break;
        }
        apply(&line)?;
        cursor.offset += bytes as u64;
    }
    Ok(cursor.offset == size && fs::metadata(path)?.len() == size)
}

fn apply_record(sessions: &mut HashMap<String, Session>, line: &[u8]) -> io::Result<i64> {
    let record: Record = serde_json::from_slice(line).map_err(|_| io::ErrorKind::InvalidData)?;
    let at = DateTime::parse_from_rfc3339(&record.timestamp)
        .map_err(|_| io::ErrorKind::InvalidData)?
        .timestamp_millis();
    let Some(id) = record
        .session_id
        .filter(|s| !s.is_empty() && s.len() <= 256)
    else {
        return Ok(at);
    };
    let Some(event) = record.event else {
        return Ok(at);
    };
    let mapping = record.module == "bootstrap.zcode_protocol"
        && event == "zcode_protocol.session_create.started"
        && record.context.workspace_key.is_some();
    let started = record.module == "core.runtime" && event == "turn.started";
    let ended = record.module == "core.runtime"
        && matches!(event.as_str(), "turn.completed" | "turn.failed");
    let unloaded = record.module.starts_with("bootstrap")
        && event == "zcode_protocol.session.resident_deactivated";
    let progress = matches!(
        record.module.as_str(),
        "core.runtime" | "adapters.model" | "core.tool.executor"
    ) && record.turn_id.is_some()
        && sessions
            .get(&id)
            .and_then(|s| s.turn.as_ref())
            .is_some_and(|t| t.active && t.id == record.turn_id);
    if !mapping && !started && !ended && !unloaded && !progress {
        return Ok(at);
    }
    let session = sessions.entry(id).or_default();
    if progress && let Some(turn) = &mut session.turn {
        turn.last_seen = turn.last_seen.max(at);
    }
    if mapping && session.workspace.as_ref().is_none_or(|(old, _)| at >= *old) {
        session.workspace = record
            .context
            .workspace_key
            .map(|workspace| (at, workspace));
    }
    if started || ended || unloaded {
        if session.turn.as_ref().is_some_and(|old| old.at > at) {
            return Ok(at);
        }
        if ended
            && session
                .turn
                .as_ref()
                .is_some_and(|old| old.active && old.id != record.turn_id)
        {
            return Ok(at);
        }
        if (started || ended) && record.turn_id.as_ref().is_none_or(String::is_empty) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        session.turn = Some(Turn {
            id: record.turn_id,
            at,
            active: started,
            root: record.context.parent_session_id.is_none(),
            last_seen: at,
        });
    }
    Ok(at)
}

impl ZCodeActivity {
    pub(super) fn probe(&mut self) -> Reading<Activity> {
        let Some(home) = tool_home("ZCODE_HOME", ".zcode") else {
            return Reading::Unknown;
        };
        let instant = Instant::now();
        if self.home.as_ref() != Some(&home)
            || self
                .last_probe
                .is_some_and(|last| instant.duration_since(last) > Duration::from_secs(15))
        {
            *self = Self {
                home: Some(home.clone()),
                ..Self::default()
            };
        }
        self.last_probe = Some(instant);
        match self.count(&home) {
            Ok(Some(running)) => Reading::Observed {
                value: Activity {
                    running,
                    waiting: None,
                    lower_bound: true,
                },
                at: SystemTime::now(),
                valid_for: Duration::from_secs(15),
            },
            Ok(_) => Reading::Unknown,
            Err(error) => {
                self.cursors.clear();
                self.owners.clear();
                self.sessions.clear();
                self.latest_cli_at = 0;
                if error.kind() == io::ErrorKind::NotFound {
                    Reading::Unknown
                } else {
                    Reading::Failed
                }
            }
        }
    }

    fn count(&mut self, home: &Path) -> io::Result<Option<u32>> {
        let system = runtimes();
        let runtime = runtime_metadata(&system);
        if runtime.is_empty() {
            self.owners.clear();
            self.sessions.clear();
            self.cursors.clear();
            return Ok(None);
        }
        let now = Local::now().timestamp_millis();
        self.owners
            .retain(|_, o| runtime.iter().any(|r| valid_owner(o, r, now)));
        let mut budget = BUDGET;
        for (_, path) in daily_files(&home.join("v2/logs"), "", ".log")? {
            let owners = &mut self.owners;
            let complete = scan(
                &path,
                self.cursors.entry(path.clone()).or_default(),
                &mut budget,
                |line| {
                    if let Some(owner) = owner(line)
                        && runtime.iter().any(|r| valid_owner(&owner, r, now))
                        && owners.get(&owner.pid).is_none_or(|old| owner.at >= old.at)
                    {
                        owners.insert(owner.pid, owner);
                    }
                    Ok(())
                },
            )?;
            if !complete {
                return Ok(None);
            }
            if runtime.iter().all(|r| self.owners.contains_key(&r.pid)) {
                break;
            }
        }
        if runtime.iter().any(|r| !self.owners.contains_key(&r.pid)) {
            return Ok(None);
        }
        let born = runtime
            .iter()
            .map(|r| r.born)
            .min()
            .ok_or(io::ErrorKind::InvalidData)?;
        let first_date = DateTime::from_timestamp_millis(born)
            .ok_or(io::ErrorKind::InvalidData)?
            .with_timezone(&Local)
            .date_naive();
        let files: Vec<_> = daily_files(&home.join("cli/log"), "zcode-", ".jsonl")?
            .into_iter()
            .filter(|(date, _)| *date >= first_date)
            .collect();
        if files.is_empty() {
            return Ok(None);
        }
        for (_, path) in files {
            let sessions = &mut self.sessions;
            let latest = &mut self.latest_cli_at;
            if !scan(
                &path,
                self.cursors.entry(path.clone()).or_default(),
                &mut budget,
                |line| {
                    *latest = (*latest).max(apply_record(sessions, line)?);
                    Ok(())
                },
            )? {
                return Ok(None);
            }
        }
        let db = Connection::open_with_flags(
            home.join("cli/db/db.sqlite"),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(io::Error::other)?;
        db.busy_timeout(Duration::from_millis(100))
            .map_err(io::Error::other)?;
        db.pragma_update(None, "query_only", true)
            .map_err(io::Error::other)?;
        exclude_finished(&db, &mut self.sessions).map_err(io::Error::other)?;
        let running = verified_count(&self.sessions, &self.owners, &runtime, now, |pid| {
            system
                .process(Pid::from_u32(pid))
                .is_some_and(|p| p.exists())
        });
        if running == 0
            && !confirmed_idle(
                &self.sessions,
                &self.owners,
                &runtime,
                now,
                self.latest_cli_at,
                |pid| {
                    system
                        .process(Pid::from_u32(pid))
                        .is_some_and(|p| p.exists())
                },
            )
        {
            return Ok(None);
        }
        Ok(Some(running))
    }
}

// This table is an independent terminal fact, not a source of active tasks.
// It also prevents a lost completion log from retaining an already-finished turn.
fn exclude_finished(
    db: &Connection,
    sessions: &mut HashMap<String, Session>,
) -> rusqlite::Result<()> {
    let mut query = db.prepare("SELECT completed_at FROM turn_usage WHERE session_id=?1 AND turn_id=?2 AND status IN ('completed','error','cancelled')")?;
    for (sid, session) in sessions {
        let Some(turn) = &mut session.turn else {
            continue;
        };
        if !turn.active || !turn.root {
            continue;
        }
        let Some(id) = &turn.id else {
            continue;
        };
        let completed: Option<i64> = query
            .query_row([sid.as_str(), id.as_str()], |r| r.get(0))
            .optional()?;
        if let Some(completed) = completed
            && completed >= turn.at
        {
            turn.active = false;
            turn.at = turn.at.max(completed);
        }
    }
    Ok(())
}

fn verified_count(
    sessions: &HashMap<String, Session>,
    owners: &HashMap<u32, Owner>,
    runtime: &[Runtime],
    now: i64,
    mut alive: impl FnMut(u32) -> bool,
) -> u32 {
    let mut running = 0;
    for session in sessions.values() {
        let (Some(turn), Some((mapped_at, workspace))) = (&session.turn, &session.workspace) else {
            continue;
        };
        if !turn.active
            || !turn.root
            || turn.at > now
            || *mapped_at > turn.at
            || turn.last_seen > now
            || now - turn.last_seen >= HEALTH_MS
        {
            continue;
        }
        let owners: Vec<_> = owners
            .values()
            .filter(|o| o.workspace_key == *workspace)
            .collect();
        if owners.len() != 1 {
            continue;
        }
        let owner = owners[0];
        if owner.at > turn.at || !runtime.iter().any(|r| valid_owner(owner, r, now)) {
            continue;
        }
        if alive(owner.pid) && alive(owner.host_pid) {
            running += 1;
        }
    }
    running
}

// Zero is only a scoped observation after explicit terminal events, with a live,
// caught-up source. Missing mappings, stalled turns and missing data stay unknown.
fn confirmed_idle(
    sessions: &HashMap<String, Session>,
    owners: &HashMap<u32, Owner>,
    runtime: &[Runtime],
    now: i64,
    latest: i64,
    mut alive: impl FnMut(u32) -> bool,
) -> bool {
    if latest > now
        || now - latest >= HEALTH_MS
        || sessions
            .values()
            .any(|s| s.turn.as_ref().is_some_and(|t| t.active && t.root))
    {
        return false;
    }
    sessions.values().any(|session| {
        let (Some(turn), Some((mapped, workspace))) = (&session.turn, &session.workspace) else {
            return false;
        };
        turn.root
            && !turn.active
            && *mapped <= turn.at
            && owners
                .values()
                .filter(|o| o.workspace_key == *workspace)
                .count()
                == 1
            && owners.values().any(|owner| {
                owner.workspace_key == *workspace
                    && owner.at <= turn.at
                    && runtime.iter().any(|r| valid_owner(owner, r, now))
                    && alive(owner.pid)
                    && alive(owner.host_pid)
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn emit(
        sessions: &mut HashMap<String, Session>,
        event: &str,
        sid: &str,
        turn: &str,
        at: i64,
        child: bool,
    ) {
        let context = if event == "zcode_protocol.session_create.started" {
            serde_json::json!({"workspaceKey":"workspace"})
        } else if child {
            serde_json::json!({"parentSessionId":"root"})
        } else {
            serde_json::json!({})
        };
        let line = serde_json::json!({
            "timestamp": DateTime::<Utc>::from_timestamp_millis(at).unwrap().to_rfc3339(),
            "event":event, "module":if event.starts_with("turn.") {"core.runtime"} else {"bootstrap.zcode_protocol"},
            "sessionId":sid, "turnId":turn, "context":context,
            "message":"synthetic content ignored by the parser"
        });
        apply_record(sessions, &serde_json::to_vec(&line).unwrap()).unwrap();
    }

    fn fixture() -> (HashMap<u32, Owner>, Vec<Runtime>) {
        let exe = std::env::current_exe().unwrap();
        (
            HashMap::from([(
                42,
                Owner {
                    pid: 42,
                    workspace_key: "workspace".into(),
                    command: exe.clone(),
                    host_pid: 7,
                    at: 1000,
                },
            )]),
            vec![Runtime {
                pid: 42,
                parent: 7,
                born: 1000,
                exe,
            }],
        )
    }

    #[test]
    fn running_tasks_are_deduplicated_and_internal_agents_are_excluded() {
        let mut sessions = HashMap::new();
        let (owners, runtime) = fixture();
        emit(
            &mut sessions,
            "zcode_protocol.session_create.started",
            "root",
            "",
            1100,
            false,
        );
        emit(&mut sessions, "turn.started", "root", "one", 1200, false);
        emit(&mut sessions, "turn.started", "root", "one", 1200, false);
        emit(
            &mut sessions,
            "zcode_protocol.session_create.started",
            "child",
            "",
            1100,
            false,
        );
        emit(
            &mut sessions,
            "turn.started",
            "child",
            "child-turn",
            1300,
            true,
        );
        assert_eq!(
            verified_count(&sessions, &owners, &runtime, 2000, |_| true),
            1
        );
        assert_eq!(
            verified_count(&sessions, &owners, &runtime, 2000, |_| false),
            0
        );
        assert_eq!(
            verified_count(&sessions, &owners, &runtime, 2000, |pid| pid != 7),
            0
        );
        assert_eq!(
            verified_count(&sessions, &HashMap::new(), &runtime, 2000, |_| true),
            0
        );
    }

    #[test]
    fn completion_cancellation_and_unload_remove_a_task_without_late_event_resurrection() {
        let (owners, runtime) = fixture();
        for terminal in [
            "turn.completed",
            "turn.failed",
            "zcode_protocol.session.resident_deactivated",
        ] {
            let mut sessions = HashMap::new();
            emit(
                &mut sessions,
                "zcode_protocol.session_create.started",
                "root",
                "",
                1100,
                false,
            );
            emit(&mut sessions, "turn.started", "root", "one", 1200, false);
            emit(&mut sessions, "turn.started", "root", "two", 1400, false);
            emit(&mut sessions, "turn.completed", "root", "one", 1500, false);
            assert_eq!(
                verified_count(&sessions, &owners, &runtime, 2000, |_| true),
                1
            );
            emit(&mut sessions, terminal, "root", "two", 1600, false);
            emit(&mut sessions, "turn.started", "root", "two", 1400, false);
            assert_eq!(
                verified_count(&sessions, &owners, &runtime, 2000, |_| true),
                0
            );
        }
    }

    #[test]
    fn process_reuse_wrong_parents_paths_and_ambiguous_ownership_are_not_activity() {
        let (mut owners, mut runtime) = fixture();
        let mut sessions = HashMap::new();
        emit(
            &mut sessions,
            "zcode_protocol.session_create.started",
            "root",
            "",
            1100,
            false,
        );
        emit(&mut sessions, "turn.started", "root", "one", 1200, false);
        runtime[0].born = 1300;
        assert_eq!(
            verified_count(&sessions, &owners, &runtime, 2000, |_| true),
            0
        );
        runtime[0].born = 1000;
        runtime[0].parent = 8;
        assert_eq!(
            verified_count(&sessions, &owners, &runtime, 2000, |_| true),
            0
        );
        runtime[0].parent = 7;
        owners.get_mut(&42).unwrap().command = PathBuf::from("different");
        assert_eq!(
            verified_count(&sessions, &owners, &runtime, 2000, |_| true),
            0
        );
        owners.get_mut(&42).unwrap().command = runtime[0].exe.clone();
        owners.insert(
            43,
            Owner {
                pid: 43,
                workspace_key: "workspace".into(),
                command: runtime[0].exe.clone(),
                host_pid: 7,
                at: 1000,
            },
        );
        assert_eq!(
            verified_count(&sessions, &owners, &runtime, 2000, |_| true),
            0
        );
    }

    #[test]
    fn installed_desktop_owner_format_is_strict_and_timezone_aware() {
        let exe = std::env::current_exe().unwrap();
        let json = serde_json::json!({"pid":42,"workspaceKey":"workspace","command":exe});
        let line = format!(
            "[2026-10-26 09:34:46.117] [info] [host] [2026-10-26 09:34:46.111] [pid:7] [zcode-agent] ZCode agent process started {json}"
        );
        let parsed = owner(line.as_bytes()).unwrap();
        assert_eq!(parsed.pid, 42);
        assert_eq!(parsed.host_pid, 7);
        assert_eq!(
            parsed.at,
            Local
                .with_ymd_and_hms(2026, 10, 26, 9, 34, 46)
                .single()
                .unwrap()
                .timestamp_millis()
                + 111
        );
        assert!(
            owner(format!("unrelated message ZCode agent process started {json}").as_bytes())
                .is_none()
        );
    }

    #[test]
    fn incremental_reader_waits_for_complete_lines_and_rejects_replacement_and_bad_json() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("zcode-cursor-{}.jsonl", std::process::id()));
        fs::write(&path, b"one\npar").unwrap();
        let mut cursor = Cursor::default();
        let mut budget = BUDGET;
        let mut lines = Vec::new();
        assert!(
            !scan(&path, &mut cursor, &mut budget, |line| {
                lines.push(line.to_vec());
                Ok(())
            })
            .unwrap()
        );
        assert_eq!(cursor.offset, 4);
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"tial\n")
            .unwrap();
        assert!(
            scan(&path, &mut cursor, &mut budget, |line| {
                lines.push(line.to_vec());
                Ok(())
            })
            .unwrap()
        );
        assert_eq!(lines, [b"one\n".to_vec(), b"partial\n".to_vec()]);
        fs::write(&path, b"two\npartial\n").unwrap();
        assert_eq!(
            scan(&path, &mut cursor, &mut budget, |_| Ok(()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(apply_record(&mut HashMap::new(), b"{broken}\n").is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn durable_terminal_fact_removes_a_turn_even_when_its_completion_log_is_missing() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE turn_usage(session_id TEXT, turn_id TEXT, status TEXT, completed_at INTEGER);
            INSERT INTO turn_usage VALUES('done','one','completed',1500),('cancelled','one','cancelled',1500),('error','one','error',1500),('old-record','one','completed',1000);").unwrap();
        let mut sessions = HashMap::new();
        for sid in ["done", "cancelled", "error", "still-running", "old-record"] {
            emit(
                &mut sessions,
                "zcode_protocol.session_create.started",
                sid,
                "",
                1100,
                false,
            );
            emit(&mut sessions, "turn.started", sid, "one", 1200, false);
        }
        exclude_finished(&db, &mut sessions).unwrap();
        let (owners, runtime) = fixture();
        assert_eq!(
            verified_count(&sessions, &owners, &runtime, 2000, |_| true),
            2
        );
    }

    #[test]
    fn stalled_logging_expires_activity_and_only_the_same_turn_can_refresh_it() {
        let mut sessions = HashMap::new();
        emit(
            &mut sessions,
            "zcode_protocol.session_create.started",
            "root",
            "",
            1100,
            false,
        );
        emit(&mut sessions, "turn.started", "root", "one", 1200, false);
        let (owners, runtime) = fixture();
        assert_eq!(
            verified_count(&sessions, &owners, &runtime, 121200, |_| true),
            0
        );
        emit(
            &mut sessions,
            "model.request.completed",
            "root",
            "other",
            121200,
            false,
        );
        assert_eq!(
            verified_count(&sessions, &owners, &runtime, 121200, |_| true),
            0
        );
        let line = serde_json::json!({"timestamp":DateTime::<Utc>::from_timestamp_millis(121200).unwrap().to_rfc3339(),"event":"model.request.completed","module":"adapters.model","sessionId":"root","turnId":"one"});
        apply_record(&mut sessions, &serde_json::to_vec(&line).unwrap()).unwrap();
        assert_eq!(
            verified_count(&sessions, &owners, &runtime, 121200, |_| true),
            1
        );
    }

    #[test]
    fn zero_requires_explicit_terminal_state_live_owners_and_a_healthy_source() {
        let mut sessions = HashMap::new();
        let (owners, runtime) = fixture();
        assert!(!confirmed_idle(
            &sessions,
            &owners,
            &runtime,
            2000,
            2000,
            |_| true
        ));
        emit(
            &mut sessions,
            "zcode_protocol.session_create.started",
            "root",
            "",
            1100,
            false,
        );
        emit(&mut sessions, "turn.started", "root", "one", 1200, false);
        assert!(!confirmed_idle(
            &sessions,
            &owners,
            &runtime,
            2000,
            2000,
            |_| true
        ));
        emit(&mut sessions, "turn.completed", "root", "one", 1500, false);
        assert!(confirmed_idle(
            &sessions,
            &owners,
            &runtime,
            2000,
            2000,
            |_| true
        ));
        assert!(!confirmed_idle(
            &sessions,
            &HashMap::new(),
            &runtime,
            2000,
            2000,
            |_| true
        ));
        assert!(!confirmed_idle(
            &sessions,
            &owners,
            &runtime,
            2000,
            2000,
            |_| false
        ));
        assert!(!confirmed_idle(
            &sessions,
            &owners,
            &runtime,
            122000,
            2000,
            |_| true
        ));
    }
}
