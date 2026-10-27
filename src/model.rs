use std::time::{Duration, SystemTime};

/// Unknown, unsupported and failed reads are distinct from a measured zero.
#[derive(Clone, Debug, PartialEq)]
pub enum Reading<T> {
    Unknown,
    Unsupported,
    Failed,
    Observed {
        value: T,
        at: SystemTime,
        valid_for: Duration,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Freshness {
    Unknown,
    Unsupported,
    Failed,
    Fresh,
    Stale,
}

impl<T> Reading<T> {
    pub fn freshness(&self, now: SystemTime) -> Freshness {
        match self {
            Self::Unknown => Freshness::Unknown,
            Self::Unsupported => Freshness::Unsupported,
            Self::Failed => Freshness::Failed,
            Self::Observed { at, valid_for, .. } => match now.duration_since(*at) {
                Ok(age) if age < *valid_for => Freshness::Fresh,
                // A future timestamp after a clock change is not evidence of freshness.
                _ => Freshness::Stale,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct QuotaWindow {
    pub bucket: String,
    pub window_minutes: Option<u64>,
    pub used_percent: Option<f64>,
    pub resets_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Activity {
    pub running: u32,
    pub waiting: Option<u32>,
    /// Finite-retention observations can confirm a minimum, not global coverage.
    pub lower_bound: bool,
}

/// Source discovery says nothing about whether a client is running or signed in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SourceState {
    Found,
    Missing,
    Unreadable,
}

#[derive(Clone, Debug)]
pub struct ToolSnapshot {
    pub id: &'static str,
    pub name: &'static str,
    pub source: SourceState,
    pub quota: Reading<Vec<QuotaWindow>>,
    pub activity: Reading<Activity>,
    pub checked_at: SystemTime,
    /// Static, sanitized provenance/error text. Never upstream response bodies.
    pub quota_note: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_a_real_observation_and_expiry_is_explicit() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let reading = Reading::Observed {
            value: 0,
            at,
            valid_for: Duration::from_secs(60),
        };
        assert_eq!(reading.freshness(at), Freshness::Fresh);
        assert_eq!(
            reading.freshness(at + Duration::from_secs(60)),
            Freshness::Stale
        );
        assert_eq!(
            reading.freshness(at - Duration::from_secs(1)),
            Freshness::Stale
        );
        assert_eq!(Reading::<u32>::Unknown.freshness(at), Freshness::Unknown);
        assert_eq!(Reading::<u32>::Failed.freshness(at), Freshness::Failed);
        assert_eq!(
            Reading::<u32>::Unsupported.freshness(at),
            Freshness::Unsupported
        );
    }
}
