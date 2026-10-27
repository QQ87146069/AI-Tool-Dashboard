use crate::model::{QuotaWindow, Reading};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant, SystemTime};
use zeroize::Zeroizing;

const REFRESH: Duration = Duration::from_secs(60);
pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Clone, Copy)]
pub(super) enum Endpoint {
    Codex,
    BigModel,
}

impl Endpoint {
    fn url(self) -> &'static str {
        match self {
            Self::Codex => "https://chatgpt.com/backend-api/wham/usage",
            Self::BigModel => "https://bigmodel.cn/api/monitor/usage/quota/limit",
        }
    }
}

// Intentionally no Debug/Serialize implementation for credentials or HTTP replies.
pub(super) struct Access {
    pub endpoint: Endpoint,
    pub authorization: Zeroizing<String>,
    pub account: Option<Zeroizing<String>>,
    pub context: Zeroizing<String>,
}

impl Access {
    pub fn scope(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        for part in [
            self.endpoint.url(),
            self.authorization.as_str(),
            self.account.as_deref().map(|s| s.as_str()).unwrap_or(""),
            self.context.as_str(),
        ] {
            hash.update((part.len() as u64).to_le_bytes());
            hash.update(part.as_bytes());
        }
        hash.finalize().into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Issue {
    NotConfigured,
    Unsupported,
    Credentials,
    Network,
    Rejected,
    InvalidResponse,
    ScopeChanged,
}

impl Issue {
    pub fn note(self) -> &'static str {
        match self {
            Self::NotConfigured => "未发现当前登录信息 · 额度未知",
            Self::Unsupported => "当前账号或 provider 的额度路径待验证",
            Self::Credentials => "登录信息不可读或已失效 · 请在客户端检查登录",
            Self::Network => "额度连接失败 · 将自动退避重试",
            Self::Rejected => "额度请求被拒绝 · 请在客户端检查登录/权限",
            Self::InvalidResponse => "额度响应不完整或格式变化 · 未推断为零",
            Self::ScopeChanged => "查询期间账号发生变化 · 已丢弃旧结果",
        }
    }
}

#[derive(Clone)]
pub(super) struct QuotaHttp(ureq::Agent);

impl QuotaHttp {
    pub fn new() -> Self {
        Self(
            ureq::Agent::config_builder()
                .https_only(true)
                .max_redirects(0)
                .http_status_as_error(false)
                .timeout_global(Some(REQUEST_TIMEOUT))
                .tls_config(
                    ureq::tls::TlsConfig::builder()
                        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                        .build(),
                )
                .user_agent("ai-tool-dashboard/0.1.0")
                .build()
                .new_agent(),
        )
    }

    pub fn get(&self, access: &Access) -> Result<Vec<u8>, Issue> {
        let mut request = self
            .0
            .get(access.endpoint.url())
            .header("Authorization", access.authorization.as_str());
        if let Some(account) = &access.account {
            request = request.header("ChatGPT-Account-Id", account.as_str());
        }
        let mut response = request.call().map_err(|_| Issue::Network)?;
        match response.status().as_u16() {
            200 => response
                .body_mut()
                .with_config()
                .limit(1024 * 1024)
                .read_to_vec()
                .map_err(|_| Issue::InvalidResponse),
            401 | 403 => Err(Issue::Rejected),
            _ => Err(Issue::Network),
        }
    }
}

pub(super) struct QuotaPoll {
    scope: Option<[u8; 32]>,
    reading: Reading<Vec<QuotaWindow>>,
    note: &'static str,
    next: Option<Instant>,
    failures: u32,
}

impl Default for QuotaPoll {
    fn default() -> Self {
        Self {
            scope: None,
            reading: Reading::Unknown,
            note: Issue::NotConfigured.note(),
            next: None,
            failures: 0,
        }
    }
}

impl QuotaPoll {
    pub fn poll(
        &mut self,
        now: Instant,
        wall: SystemTime,
        load: impl Fn() -> Result<Access, Issue>,
        fetch: impl FnOnce(&Access) -> Result<Vec<QuotaWindow>, Issue>,
        success_note: &'static str,
    ) -> (Reading<Vec<QuotaWindow>>, &'static str) {
        let access = match load() {
            Ok(access) => access,
            Err(issue) => {
                // Losing credentials/logging out invalidates previous account data immediately.
                *self = Self::default();
                self.note = issue.note();
                self.reading = match issue {
                    Issue::NotConfigured => Reading::Unknown,
                    Issue::Unsupported => Reading::Unsupported,
                    _ => Reading::Failed,
                };
                return (self.reading.clone(), self.note);
            }
        };
        let scope = access.scope();
        if self.scope != Some(scope) {
            *self = Self::default();
            self.scope = Some(scope);
        }
        if self.next.is_none_or(|next| now >= next) {
            let result = fetch(&access);
            // Do not publish a response for an account switched while the request was in flight.
            if load().map(|current| current.scope()).ok() != Some(scope) {
                *self = Self::default();
                self.note = Issue::ScopeChanged.note();
                return (Reading::Unknown, self.note);
            }
            match result {
                Ok(windows) if windows.iter().any(|window| window.used_percent.is_some()) => {
                    let valid_for = windows
                        .iter()
                        .filter_map(|window| window.resets_at)
                        .filter_map(|reset| {
                            SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(reset))
                        })
                        .map(|reset| reset.duration_since(wall).unwrap_or_default())
                        .fold(Duration::from_secs(90), Duration::min);
                    self.reading = Reading::Observed {
                        value: windows,
                        at: wall,
                        valid_for,
                    };
                    self.note = success_note;
                    self.failures = 0;
                }
                result => {
                    self.note = result.err().unwrap_or(Issue::InvalidResponse).note();
                    self.failures = self.failures.saturating_add(1);
                    self.reading = match &self.reading {
                        Reading::Observed { value, at, .. } => Reading::Observed {
                            value: value.clone(),
                            at: *at,
                            valid_for: Duration::ZERO,
                        },
                        _ => Reading::Failed,
                    };
                }
            }
            let delay = REFRESH
                .saturating_mul(1u32 << self.failures.saturating_sub(1).min(4))
                .min(Duration::from_secs(900));
            self.next = Some(now + delay);
        }
        (self.reading.clone(), self.note)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Freshness;
    use std::cell::Cell;

    fn access(account: &str) -> Access {
        Access {
            endpoint: Endpoint::Codex,
            authorization: Zeroizing::new("synthetic".into()),
            account: None,
            context: Zeroizing::new(account.into()),
        }
    }
    fn windows() -> Vec<QuotaWindow> {
        vec![QuotaWindow {
            bucket: "test".into(),
            window_minutes: Some(300),
            used_percent: Some(0.0),
            resets_at: None,
        }]
    }

    #[test]
    fn throttles_manual_refresh_and_backs_off_failures_without_faking_zero() {
        let mut poll = QuotaPoll::default();
        let now = Instant::now();
        let wall = SystemTime::now();
        let (reading, _) = poll.poll(now, wall, || Ok(access("A")), |_| Ok(windows()), "ok");
        assert_eq!(reading.freshness(wall), Freshness::Fresh);
        poll.poll(
            now + Duration::from_secs(1),
            wall,
            || Ok(access("A")),
            |_| panic!("must coalesce"),
            "ok",
        );
        let (reading, _) = poll.poll(
            now + REFRESH,
            wall,
            || Ok(access("A")),
            |_| Err(Issue::Network),
            "ok",
        );
        assert_eq!(reading.freshness(wall), Freshness::Stale);
        poll.poll(
            now + REFRESH * 2,
            wall,
            || Ok(access("A")),
            |_| Err(Issue::Network),
            "ok",
        );
        poll.poll(
            now + REFRESH * 3,
            wall,
            || Ok(access("A")),
            |_| panic!("must back off"),
            "ok",
        );
    }

    #[test]
    fn account_switch_clears_cached_values_even_during_request_or_logout() {
        let mut poll = QuotaPoll::default();
        let now = Instant::now();
        let wall = SystemTime::now();
        poll.poll(now, wall, || Ok(access("A")), |_| Ok(windows()), "ok");
        let (reading, _) = poll.poll(now, wall, || Ok(access("B")), |_| Err(Issue::Network), "ok");
        assert_eq!(reading, Reading::Failed);
        let count = Cell::new(0);
        let (reading, _) = poll.poll(
            now,
            wall,
            || {
                count.set(count.get() + 1);
                Ok(access(if count.get() == 1 { "C" } else { "D" }))
            },
            |_| Ok(windows()),
            "ok",
        );
        assert_eq!(reading, Reading::Unknown);
        let (reading, _) = poll.poll(
            now,
            wall,
            || Err(Issue::NotConfigured),
            |_| panic!("must not fetch"),
            "ok",
        );
        assert_eq!(reading, Reading::Unknown);
    }

    #[test]
    fn validity_never_outlives_a_quota_reset_and_empty_data_is_not_zero() {
        let mut poll = QuotaPoll::default();
        let now = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let mut value = windows();
        value[0].resets_at = Some(1010);
        let (reading, _) = poll.poll(now, wall, || Ok(access("A")), |_| Ok(value), "ok");
        assert_eq!(
            reading.freshness(wall + Duration::from_secs(10)),
            Freshness::Stale
        );
        let (reading, _) = poll.poll(now, wall, || Ok(access("B")), |_| Ok(vec![]), "ok");
        assert_eq!(reading, Reading::Failed);
    }
}
