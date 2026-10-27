use super::{ToolAdapter, discover, tool_home};
use super::{
    credentials,
    quota::{Issue, QuotaHttp, QuotaPoll},
};
use crate::model::{QuotaWindow, ToolSnapshot};
use serde::Deserialize;
use std::time::{Instant, SystemTime};

pub struct ZCode {
    http: QuotaHttp,
    poll: QuotaPoll,
    activity: super::zcode_activity::ZCodeActivity,
}

impl ZCode {
    pub(super) fn new(http: QuotaHttp) -> Self {
        Self {
            http,
            poll: QuotaPoll::default(),
            activity: super::zcode_activity::ZCodeActivity::default(),
        }
    }
}

impl ToolAdapter for ZCode {
    fn probe(&mut self) -> ToolSnapshot {
        let (quota, quota_note) = self.poll.poll(
            Instant::now(),
            SystemTime::now(),
            credentials::zcode,
            |access| parse_usage(&self.http.get(access)?),
            "BigModel 个人订阅额度 · 只读查询",
        );
        ToolSnapshot {
            id: "zcode",
            name: "ZCode",
            source: discover(
                tool_home("ZCODE_HOME", ".zcode").map(|p| p.join("v2/tasks-index.sqlite")),
            ),
            quota,
            activity: self.activity.probe(),
            checked_at: SystemTime::now(),
            quota_note,
        }
    }
}

#[derive(Deserialize)]
struct Envelope {
    code: Option<u16>,
    success: Option<bool>,
    data: Option<Payload>,
}
#[derive(Deserialize)]
struct Payload {
    limits: Option<Vec<Limit>>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Limit {
    r#type: Option<String>,
    unit: Option<u32>,
    number: Option<u64>,
    percentage: Option<f64>,
    next_reset_time: Option<u64>,
}

fn parse_usage(bytes: &[u8]) -> Result<Vec<QuotaWindow>, Issue> {
    let envelope: Envelope = serde_json::from_slice(bytes).map_err(|_| Issue::InvalidResponse)?;
    if envelope.success == Some(false) || !matches!(envelope.code, Some(0 | 200)) {
        return Err(Issue::InvalidResponse);
    }
    let limits = envelope
        .data
        .and_then(|data| data.limits)
        .ok_or(Issue::InvalidResponse)?;
    // TIME_LIMIT is a separate tool/search meter, not the subscription credit window.
    Ok(limits
        .into_iter()
        .filter(|limit| {
            matches!(
                limit.r#type.as_deref(),
                Some("CREDIT_LIMIT" | "TOKENS_LIMIT")
            )
        })
        .map(|limit| QuotaWindow {
            bucket: "BigModel".into(),
            used_percent: limit
                .percentage
                .filter(|p| p.is_finite() && (0.0..=100.0).contains(p)),
            window_minutes: match (limit.unit, limit.number) {
                (Some(3), Some(n)) if n > 0 => n.checked_mul(60),
                (Some(6), Some(n)) if n > 0 => n.checked_mul(10080),
                _ => None,
            },
            resets_at: limit
                .next_reset_time
                .filter(|ms| *ms >= 1000)
                .map(|ms| ms / 1000),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn percentage_is_used_and_reset_is_milliseconds_without_inventing_missing_values() {
        let windows = parse_usage(br#"{"code":200,"success":true,"data":{"limits":[{"type":"CREDIT_LIMIT","unit":3,"number":5,"percentage":4,"nextResetTime":2000000000123},{"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":49},{"type":"CREDIT_LIMIT","percentage":null},{"type":"TIME_LIMIT","percentage":80}]}}"#).unwrap();
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].used_percent, Some(4.0));
        assert_eq!(windows[0].window_minutes, Some(300));
        assert_eq!(windows[0].resets_at, Some(2000000000));
        assert_eq!(windows[1].window_minutes, Some(10080));
        assert_eq!(windows[2].used_percent, None);
        assert_eq!(windows[2].window_minutes, None);
        assert!(parse_usage(br#"{"code":401,"data":{"limits":[]}}"#).is_err());
        assert!(parse_usage(br#"{"code":200,"success":false,"data":{"limits":[]}}"#).is_err());
    }
}
