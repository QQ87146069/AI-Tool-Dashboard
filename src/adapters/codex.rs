use super::{ToolAdapter, discover, tool_home};
use super::{
    credentials,
    quota::{Issue, QuotaHttp, QuotaPoll},
};
use crate::model::{QuotaWindow, ToolSnapshot};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    time::{Instant, SystemTime},
};

pub struct Codex {
    http: QuotaHttp,
    poll: QuotaPoll,
    activity: super::codex_activity::CodexActivity,
}

impl Codex {
    pub(super) fn new(http: QuotaHttp) -> Self {
        Self {
            http,
            poll: QuotaPoll::default(),
            activity: super::codex_activity::CodexActivity::default(),
        }
    }
}

impl ToolAdapter for Codex {
    fn probe(&mut self) -> ToolSnapshot {
        let (quota, quota_note) = self.poll.poll(
            Instant::now(),
            SystemTime::now(),
            credentials::codex,
            |access| parse_usage(&self.http.get(access)?),
            "ChatGPT 账号订阅额度 · 只读查询",
        );
        ToolSnapshot {
            id: "codex",
            name: "Codex",
            source: discover(tool_home("CODEX_HOME", ".codex").map(|p| p.join("state_5.sqlite"))),
            quota,
            activity: self.activity.probe(),
            checked_at: SystemTime::now(),
            quota_note,
        }
    }
}

// app-server protocol parser retained independently from the direct HTTP reader.
// A new app-server is never started or assumed to observe the desktop's tasks.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitsResponse {
    rate_limits_by_limit_id: Option<BTreeMap<String, Bucket>>,
    rate_limits: Option<Bucket>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bucket {
    limit_id: Option<String>,
    primary: Option<Window>,
    secondary: Option<Window>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Window {
    used_percent: Option<f64>,
    window_duration_mins: Option<u64>,
    resets_at: Option<u64>,
}

impl RateLimitsResponse {
    pub fn windows(self) -> Vec<QuotaWindow> {
        let buckets = match self.rate_limits_by_limit_id {
            // An explicitly empty multi-bucket response is authoritative.
            Some(buckets) => buckets,
            None => self
                .rate_limits
                .map(|bucket| {
                    BTreeMap::from([(
                        bucket.limit_id.clone().unwrap_or_else(|| "codex".into()),
                        bucket,
                    )])
                })
                .unwrap_or_default(),
        };
        buckets
            .into_iter()
            .flat_map(|(id, bucket)| {
                [bucket.primary, bucket.secondary]
                    .into_iter()
                    .flatten()
                    .map(move |window| QuotaWindow {
                        bucket: id.clone(),
                        window_minutes: window.window_duration_mins.filter(|minutes| *minutes > 0),
                        used_percent: window.used_percent.filter(|percent| {
                            percent.is_finite() && (0.0..=100.0).contains(percent)
                        }),
                        resets_at: window.resets_at.filter(|timestamp| *timestamp > 0),
                    })
            })
            .collect()
    }
}

#[derive(Deserialize)]
struct UsagePayload {
    rate_limit: Option<UsageLimit>,
    additional_rate_limits: Option<Vec<AdditionalLimit>>,
}
#[derive(Deserialize)]
struct AdditionalLimit {
    metered_feature: String,
    rate_limit: Option<UsageLimit>,
}
#[derive(Deserialize)]
struct UsageLimit {
    primary_window: Option<UsageWindow>,
    secondary_window: Option<UsageWindow>,
}
#[derive(Deserialize)]
struct UsageWindow {
    used_percent: Option<f64>,
    limit_window_seconds: Option<u64>,
    reset_at: Option<u64>,
}

fn parse_usage(bytes: &[u8]) -> Result<Vec<QuotaWindow>, Issue> {
    let payload: UsagePayload =
        serde_json::from_slice(bytes).map_err(|_| Issue::InvalidResponse)?;
    let mut buckets = vec![("codex".into(), payload.rate_limit)];
    buckets.extend(
        payload
            .additional_rate_limits
            .unwrap_or_default()
            .into_iter()
            .map(|limit| (limit.metered_feature, limit.rate_limit)),
    );
    Ok(buckets
        .into_iter()
        .filter(|(id, _)| {
            !id.is_empty()
                && id.len() <= 80
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
        })
        .flat_map(|(bucket, limit)| {
            limit.into_iter().flat_map(move |limit| {
                let bucket = bucket.clone();
                [limit.primary_window, limit.secondary_window]
                    .into_iter()
                    .flatten()
                    .map(move |window| QuotaWindow {
                        bucket: bucket.clone(),
                        used_percent: window
                            .used_percent
                            .filter(|p| p.is_finite() && (0.0..=100.0).contains(p)),
                        window_minutes: window
                            .limit_window_seconds
                            .filter(|s| *s > 0 && s % 60 == 0)
                            .map(|s| s / 60),
                        resets_at: window.reset_at.filter(|s| *s > 0),
                    })
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Vec<QuotaWindow> {
        serde_json::from_str::<RateLimitsResponse>(json)
            .unwrap()
            .windows()
    }

    #[test]
    fn direct_usage_response_preserves_windows_and_additional_buckets() {
        let windows = parse_usage(br#"{"rate_limit":{"primary_window":{"used_percent":0,"limit_window_seconds":18000,"reset_at":2000000000},"secondary_window":{"used_percent":45,"limit_window_seconds":604800}},"additional_rate_limits":[{"metered_feature":"other","rate_limit":{"primary_window":{"used_percent":12,"limit_window_seconds":900}}}]}"#).unwrap();
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].used_percent, Some(0.0));
        assert_eq!(windows[1].window_minutes, Some(10080));
        assert_eq!(windows[2].bucket, "other");
        assert!(parse_usage(br#"{"rate_limit":null}"#).unwrap().is_empty());
    }

    #[test]
    fn uses_all_buckets_without_duplicating_legacy_view_or_inventing_windows() {
        let windows = parse(
            r#"{
            "rateLimitsByLimitId": {
                "codex": {"primary":{"usedPercent":0,"windowDurationMins":300},
                          "secondary":{"usedPercent":42,"windowDurationMins":10080,"resetsAt":2000000000}},
                "other": {"primary":{"usedPercent":12,"windowDurationMins":15}}
            },
            "rateLimits":{"primary":{"usedPercent":99}}
        }"#,
        );
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].used_percent, Some(0.0));
        assert_eq!(windows[1].window_minutes, Some(10080));
        assert_eq!(windows[1].resets_at, Some(2000000000));
        assert_eq!(windows[2].bucket, "other");
    }

    #[test]
    fn missing_null_and_out_of_range_fields_stay_unknown() {
        let windows = parse(
            r#"{"rateLimits":{"primary":{"usedPercent":null},"secondary":{"usedPercent":120,"windowDurationMins":0,"resetsAt":0}}}"#,
        );
        assert_eq!(windows.len(), 2);
        assert!(windows.iter().all(|window| window.used_percent.is_none()));
        assert_eq!(windows[1].window_minutes, None);
        assert_eq!(windows[1].resets_at, None);
        assert!(parse("{}").is_empty());
        assert!(
            parse(r#"{"rateLimitsByLimitId":{},"rateLimits":{"primary":{"usedPercent":5}}}"#)
                .is_empty()
        );
    }

    #[test]
    fn rejects_invalid_types_and_truncated_json() {
        assert!(
            serde_json::from_str::<RateLimitsResponse>(
                r#"{"rateLimits":{"primary":{"usedPercent":"oops"}}}"#
            )
            .is_err()
        );
        assert!(serde_json::from_str::<RateLimitsResponse>("{").is_err());
    }
}
