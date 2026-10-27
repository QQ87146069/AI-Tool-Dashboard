mod codex;
mod codex_activity;
mod credentials;
mod quota;
mod zcode;
mod zcode_activity;

pub use codex::RateLimitsResponse;

use crate::model::{SourceState, ToolSnapshot};
use directories::BaseDirs;
use std::{env, fs::File, path::PathBuf};

pub trait ToolAdapter: Send {
    /// Read only. No process spawning, authentication refresh or client mutations.
    fn probe(&mut self) -> ToolSnapshot;
}

pub fn registered() -> Vec<Box<dyn ToolAdapter>> {
    let http = quota::QuotaHttp::new();
    vec![
        Box::new(codex::Codex::new(http.clone())),
        Box::new(zcode::ZCode::new(http)),
    ]
}

fn tool_home(variable: &str, directory: &str) -> Option<PathBuf> {
    env::var_os(variable)
        .map(PathBuf::from)
        .or_else(|| BaseDirs::new().map(|base| base.home_dir().join(directory)))
}

fn discover(path: Option<PathBuf>) -> SourceState {
    let Some(path) = path else {
        return SourceState::Missing;
    };
    match File::open(path) {
        Ok(_) => SourceState::Found,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => SourceState::Missing,
        Err(_) => SourceState::Unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_source_is_missing() {
        assert_eq!(discover(None), SourceState::Missing);
    }
}
