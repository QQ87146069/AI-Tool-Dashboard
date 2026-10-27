//! Bounded, best-effort diagnostics for this application's launch only.
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct StartupLog(Option<PathBuf>);

impl StartupLog {
    pub fn new(state: Option<&Path>) -> Self {
        Self(state.map(|path| path.with_file_name("startup.log")))
    }

    pub fn record(&self, event: &str, error: Option<&str>) {
        let Some(path) = &self.0 else {
            return;
        };
        // No arguments, environment, account data or adapter errors are logged.
        let error = error.map(|text| text.chars().take(512).collect::<String>());
        let entry = serde_json::json!({
            "time": chrono::Utc::now().to_rfc3339(),
            "pid": std::process::id(),
            "event": event,
            "error": error,
        });
        let _ = (|| -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut line = serde_json::to_vec(&entry)?;
            line.push(b'\n');
            let reset =
                std::fs::metadata(path).is_ok_and(|m| m.len() + line.len() as u64 > 32 * 1024);
            OpenOptions::new()
                .create(true)
                .write(true)
                .append(!reset)
                .truncate(reset)
                .open(path)?
                .write_all(&line)
        })();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_are_bounded_valid_json_and_unwritable_logs_do_not_block_startup() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ai-dashboard-startup-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("startup.log");
        std::fs::write(&path, vec![b'x'; 32 * 1024]).unwrap();
        let log = StartupLog::new(Some(&dir.join("window.json")));
        log.record(
            "startup-or-event-loop-error",
            Some(&"启动\n错误".repeat(512)),
        );
        log.record("event-loop-ready", None);
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() <= 32 * 1024);
        let entries: Vec<serde_json::Value> = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["error"].as_str().unwrap().chars().count(), 512);
        assert_eq!(entries[1]["event"], "event-loop-ready");
        StartupLog::new(Some(&path.join("window.json"))).record("event-loop-ready", None);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
