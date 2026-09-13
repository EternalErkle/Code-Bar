use std::sync::OnceLock;

use serde::Serialize;

use crate::util::background_command;

/// 进程级共享的 blocking HTTP 客户端。
/// 每次调用都 build 一个 client 会启动一条新的运行时线程，这里只构建一次。
static HTTP_CLIENT: OnceLock<Result<reqwest::blocking::Client, String>> = OnceLock::new();

/// 上游 429 后的冷却截止时间（epoch ms）+ 最近一次成功的快照。
///
/// 卡片每次挂载都会立即刷新一次，重启和切换 runner 都会重新挂载；再叠加 3 分钟
/// 轮询，短时间内很容易把这个端点打到限流。冷却期内直接复用上次快照，既不再发
/// 请求，也不会让卡片退化成报错。
static CLAUDE_COOLDOWN_UNTIL_MS: std::sync::Mutex<i64> = std::sync::Mutex::new(0);
static CLAUDE_LAST_SNAPSHOT: std::sync::Mutex<Option<RunnerUsageSnapshot>> =
    std::sync::Mutex::new(None);

fn set_claude_cooldown_until(until_ms: i64) {
    if let Ok(mut guard) = CLAUDE_COOLDOWN_UNTIL_MS.lock() {
        *guard = until_ms;
    }
}

fn claude_cooldown_remaining_ms() -> i64 {
    let until = CLAUDE_COOLDOWN_UNTIL_MS
        .lock()
        .map(|guard| *guard)
        .unwrap_or(0);
    (until - now_ms()).max(0)
}

fn remember_claude_snapshot(snapshot: &RunnerUsageSnapshot) {
    if let Ok(mut guard) = CLAUDE_LAST_SNAPSHOT.lock() {
        *guard = Some(snapshot.clone());
    }
}

fn last_claude_snapshot() -> Option<RunnerUsageSnapshot> {
    CLAUDE_LAST_SNAPSHOT.lock().ok().and_then(|g| g.clone())
}

fn http_client() -> Result<&'static reqwest::blocking::Client, &'static str> {
    HTTP_CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .map_err(|err| err.to_string())
        })
        .as_ref()
        .map_err(|err| err.as_str())
}

#[derive(Debug, serde::Serialize)]
struct ClaudeMessageRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: [ClaudeMessage<'a>; 1],
}

#[derive(Debug, serde::Serialize)]
struct ClaudeMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// Structured usage snapshot.
///
/// Every user-visible string is either a number, an epoch-millisecond timestamp,
/// or a stable machine-readable code that the frontend translates. The backend
/// never returns pre-formatted display text.
#[derive(Debug, Clone, Serialize)]
pub struct RunnerUsageSnapshot {
    pub runner_type: String,
    /// "oauth" | "api-key" | "unsupported"
    pub source: String,
    /// Raw plan identifier reported by the provider (not translated).
    pub plan: Option<String>,
    pub five_hour_used_percent: Option<f64>,
    pub five_hour_resets_at_ms: Option<i64>,
    pub seven_day_used_percent: Option<f64>,
    pub seven_day_resets_at_ms: Option<i64>,
    pub credits_balance: Option<String>,
    pub credits_unlimited: bool,
    pub last_refreshed_at_ms: i64,
    /// Stable i18n key suffix, e.g. "noCredentials". Translated by the UI.
    pub error_code: Option<String>,
    /// Untranslated technical detail, shown only as a secondary line.
    pub error_detail: Option<String>,
}

impl RunnerUsageSnapshot {
    fn empty(runner_type: &str, source: &str) -> Self {
        Self {
            runner_type: runner_type.to_string(),
            source: source.to_string(),
            plan: None,
            five_hour_used_percent: None,
            five_hour_resets_at_ms: None,
            seven_day_used_percent: None,
            seven_day_resets_at_ms: None,
            credits_balance: None,
            credits_unlimited: false,
            last_refreshed_at_ms: now_ms(),
            error_code: None,
            error_detail: None,
        }
    }

    fn failure(runner_type: &str, code: &str, detail: Option<String>) -> Self {
        let mut snapshot = Self::empty(runner_type, "unsupported");
        snapshot.error_code = Some(code.to_string());
        snapshot.error_detail = detail;
        snapshot
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse the RFC 3339 timestamps returned by the OAuth usage endpoint
/// (e.g. `2026-09-12T20:50:00.364745+00:00`, also accepts a `Z` suffix)
/// into epoch milliseconds. Fractional seconds are ignored.
fn parse_rfc3339_ms(value: &str) -> Option<i64> {
    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() < 19 {
        return None;
    }

    let num = |range: std::ops::Range<usize>| value.get(range)?.parse::<i64>().ok();
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut millis =
        (days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000;

    // Skip optional fractional seconds, then read the offset.
    let mut index = 19;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        while bytes.get(index).map(|b| b.is_ascii_digit()).unwrap_or(false) {
            index += 1;
        }
    }

    match bytes.get(index) {
        None | Some(b'Z') | Some(b'z') => {}
        Some(sign @ (b'+' | b'-')) => {
            let offset_hour = num(index + 1..index + 3)?;
            let offset_minute = num(index + 4..index + 6)?;
            let offset = (offset_hour * 60 + offset_minute) * 60 * 1_000;
            if *sign == b'+' {
                millis -= offset;
            } else {
                millis += offset;
            }
        }
        _ => return None,
    }

    Some(millis)
}

fn parse_header_f64(response: &reqwest::blocking::Response, name: &str) -> Option<f64> {
    response.headers().get(name)?.to_str().ok()?.parse::<f64>().ok()
}

// ── Codex ────────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
struct CodexUsageResponse {
    plan_type: Option<String>,
    rate_limit: Option<CodexRateLimit>,
    credits: Option<CodexCredits>,
}

#[derive(Debug, serde::Deserialize)]
struct CodexRateLimit {
    primary_window: Option<CodexWindow>,
    secondary_window: Option<CodexWindow>,
}

#[derive(Debug, serde::Deserialize)]
struct CodexWindow {
    used_percent: Option<i64>,
    reset_at: Option<i64>,
}

#[derive(Debug, serde::Deserialize)]
struct CodexCredits {
    #[allow(dead_code)]
    has_credits: Option<bool>,
    unlimited: Option<bool>,
    balance: Option<serde_json::Value>,
}

fn fetch_codex_usage_via_http() -> RunnerUsageSnapshot {
    let auth_path = std::path::PathBuf::from(crate::util::home_dir().unwrap_or_default())
        .join(".codex")
        .join("auth.json");
    let Ok(text) = std::fs::read_to_string(&auth_path) else {
        return RunnerUsageSnapshot::failure("codex", "noCredentials", None);
    };

    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return RunnerUsageSnapshot::failure("codex", "credentialsUnreadable", None);
    };

    let access_token = value
        .get("tokens")
        .and_then(|v| v.get("access_token"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let account_id = value
        .get("tokens")
        .and_then(|v| v.get("account_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if access_token.is_empty() {
        return RunnerUsageSnapshot::failure("codex", "noCredentials", None);
    }

    let client = match http_client() {
        Ok(client) => client,
        Err(err) => {
            return RunnerUsageSnapshot::failure("codex", "requestFailed", Some(err.to_string()))
        }
    };

    let response = match client
        .get("https://chatgpt.com/backend-api/wham/usage")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Accept", "application/json")
        .header("User-Agent", "Code-Bar")
        .header("ChatGPT-Account-Id", account_id)
        .send()
    {
        Ok(response) => response,
        Err(err) => {
            return RunnerUsageSnapshot::failure("codex", "requestFailed", Some(err.to_string()))
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let code = if status.as_u16() == 401 { "unauthorized" } else { "requestFailed" };
        return RunnerUsageSnapshot::failure("codex", code, Some(status.to_string()));
    }

    let parsed: CodexUsageResponse = match response.json() {
        Ok(parsed) => parsed,
        Err(err) => {
            return RunnerUsageSnapshot::failure(
                "codex",
                "responseUnreadable",
                Some(err.to_string()),
            )
        }
    };

    let primary = parsed.rate_limit.as_ref().and_then(|r| r.primary_window.as_ref());
    let secondary = parsed.rate_limit.as_ref().and_then(|r| r.secondary_window.as_ref());

    let mut snapshot = RunnerUsageSnapshot::empty("codex", "oauth");
    snapshot.plan = parsed.plan_type;
    snapshot.five_hour_used_percent = primary.and_then(|w| w.used_percent).map(|v| v as f64);
    snapshot.five_hour_resets_at_ms = primary.and_then(|w| w.reset_at).map(|v| v * 1_000);
    snapshot.seven_day_used_percent = secondary.and_then(|w| w.used_percent).map(|v| v as f64);
    snapshot.seven_day_resets_at_ms = secondary.and_then(|w| w.reset_at).map(|v| v * 1_000);
    snapshot.credits_balance = parsed
        .credits
        .as_ref()
        .and_then(|c| c.balance.as_ref())
        .map(|v| v.as_str().map(ToString::to_string).unwrap_or_else(|| v.to_string()));
    snapshot.credits_unlimited = parsed.credits.as_ref().and_then(|c| c.unlimited).unwrap_or(false);
    snapshot
}

// ── Claude Code ──────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
struct ClaudeOauthUsageResponse {
    five_hour: Option<ClaudeOauthWindow>,
    seven_day: Option<ClaudeOauthWindow>,
}

#[derive(Debug, serde::Deserialize)]
struct ClaudeOauthWindow {
    /// Already a percentage in 0..=100 (verified against a live response).
    utilization: Option<f64>,
    resets_at: Option<String>,
}

/// Read the OAuth access token the Claude CLI stores in `~/.claude/.credentials.json`.
fn read_claude_oauth_token() -> Option<String> {
    let path = crate::util::resolve_provider_file_path("claude-code", "", ".credentials.json")?;
    let text = std::fs::read_to_string(path).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    value
        .get("claudeAiOauth")?
        .get("accessToken")?
        .as_str()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(ToString::to_string)
}

/// Query the OAuth usage endpoint the Claude CLI itself uses.
///
/// Verified on 2026-09-12 against Claude Code 2.1.269 and a live `max` subscription:
/// `GET https://api.anthropic.com/api/oauth/usage` with a bearer token from
/// `~/.claude/.credentials.json` returns 200 with `five_hour` / `seven_day`
/// objects carrying `utilization` (0..=100) and an RFC 3339 `resets_at`.
/// The literal string `oauth/usage` is also present in the shipped CLI binary.
/// Unlike the old `POST /v1/messages` probe this costs no tokens and works for
/// subscription users, who never have `ANTHROPIC_API_KEY` set.
fn fetch_claude_usage_via_oauth(token: &str) -> RunnerUsageSnapshot {
    // Deliberately NOT honouring ANTHROPIC_BASE_URL here. That variable redirects
    // model inference at an OpenAI-compatible proxy (e.g. a local relay on
    // 127.0.0.1). This endpoint is an Anthropic *account* API tied to the user's
    // OAuth session, which such proxies do not implement — pointing at one just
    // yields 404. The API-key path below still honours the override.
    let endpoint = "https://api.anthropic.com/api/oauth/usage".to_string();

    let client = match http_client() {
        Ok(client) => client,
        Err(err) => {
            return RunnerUsageSnapshot::failure("claude-code", "requestFailed", Some(err.to_string()))
        }
    };

    let response = match client
        .get(endpoint)
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Accept", "application/json")
        .header("User-Agent", "Code-Bar")
        .send()
    {
        Ok(response) => response,
        Err(err) => {
            return RunnerUsageSnapshot::failure(
                "claude-code",
                "requestFailed",
                Some(err.to_string()),
            )
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let code = match status.as_u16() {
            401 => "unauthorized",
            429 => "rateLimited",
            _ => "requestFailed",
        };
        // 429 时记录冷却截止时间：在此之前的刷新直接复用上一次快照，不再打这个
        // 端点。否则每次重试都会把限流窗口继续往后续，用量卡片长期显示报错。
        if status.as_u16() == 429 {
            let retry_after_secs = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap_or(60)
                .clamp(1, 3600) as i64
                * 1000;
            set_claude_cooldown_until(now_ms().saturating_add(retry_after_secs));
        }
        return RunnerUsageSnapshot::failure("claude-code", code, Some(status.to_string()));
    }

    let parsed: ClaudeOauthUsageResponse = match response.json() {
        Ok(parsed) => parsed,
        Err(err) => {
            return RunnerUsageSnapshot::failure(
                "claude-code",
                "responseUnreadable",
                Some(err.to_string()),
            )
        }
    };

    let mut snapshot = RunnerUsageSnapshot::empty("claude-code", "oauth");
    if let Some(window) = parsed.five_hour {
        snapshot.five_hour_used_percent = window.utilization;
        snapshot.five_hour_resets_at_ms =
            window.resets_at.as_deref().and_then(parse_rfc3339_ms);
    }
    if let Some(window) = parsed.seven_day {
        snapshot.seven_day_used_percent = window.utilization;
        snapshot.seven_day_resets_at_ms =
            window.resets_at.as_deref().and_then(parse_rfc3339_ms);
    }
    snapshot
}

/// Fallback for users authenticating with a raw API key instead of the CLI's OAuth
/// session. This spends one minimal billable request to read the rate-limit response
/// headers, so it only runs when no OAuth credentials exist.
fn fetch_claude_usage_via_headers(api_key: &str) -> RunnerUsageSnapshot {
    let base_url = std::env::var("ANTHROPIC_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "https://api.anthropic.com".to_string());
    let endpoint = format!("{}/v1/messages", base_url.trim_end_matches('/'));
    let request = ClaudeMessageRequest {
        model: "claude-haiku-4-5-20251001",
        max_tokens: 1,
        messages: [ClaudeMessage { role: "user", content: "hi" }],
    };

    let client = match http_client() {
        Ok(client) => client,
        Err(err) => {
            return RunnerUsageSnapshot::failure(
                "claude-code",
                "requestFailed",
                Some(err.to_string()),
            )
        }
    };

    let response = match client
        .post(endpoint)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&request)
        .send()
    {
        Ok(response) => response,
        Err(err) => {
            return RunnerUsageSnapshot::failure(
                "claude-code",
                "requestFailed",
                Some(err.to_string()),
            )
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let code = if status.as_u16() == 401 { "unauthorized" } else { "requestFailed" };
        return RunnerUsageSnapshot::failure("claude-code", code, Some(status.to_string()));
    }

    let mut snapshot = RunnerUsageSnapshot::empty("claude-code", "api-key");
    // These unified headers describe subscription limits and are generally absent on
    // API-key traffic, so the fields may legitimately stay None here.
    snapshot.five_hour_used_percent =
        parse_header_f64(&response, "anthropic-ratelimit-unified-5h-utilization").map(|v| v * 100.0);
    snapshot.five_hour_resets_at_ms =
        parse_header_f64(&response, "anthropic-ratelimit-unified-5h-reset").map(|v| (v * 1000.0) as i64);
    snapshot.seven_day_used_percent =
        parse_header_f64(&response, "anthropic-ratelimit-unified-7d-utilization").map(|v| v * 100.0);
    snapshot.seven_day_resets_at_ms =
        parse_header_f64(&response, "anthropic-ratelimit-unified-7d-reset").map(|v| (v * 1000.0) as i64);

    if snapshot.five_hour_used_percent.is_none() && snapshot.seven_day_used_percent.is_none() {
        snapshot.error_code = Some("noSubscriptionLimits".to_string());
    }

    snapshot
}

fn fetch_claude_usage() -> RunnerUsageSnapshot {
    // 还在 429 冷却期内就别再打上游了，直接复用上一次的快照。
    let cooldown_ms = claude_cooldown_remaining_ms();
    if cooldown_ms > 0 {
        if let Some(cached) = last_claude_snapshot() {
            return cached;
        }
        return RunnerUsageSnapshot::failure(
            "claude-code",
            "rateLimited",
            Some(format!("retry in {}s", cooldown_ms / 1000)),
        );
    }

    if let Some(token) = read_claude_oauth_token() {
        let snapshot = fetch_claude_usage_via_oauth(&token);
        if snapshot.error_code.is_none() {
            remember_claude_snapshot(&snapshot);
        }
        return snapshot;
    }

    let api_key = std::env::var("ANTHROPIC_API_KEY").ok().filter(|v| !v.trim().is_empty());
    match api_key {
        Some(api_key) => fetch_claude_usage_via_headers(&api_key),
        None => RunnerUsageSnapshot::failure("claude-code", "noCredentials", None),
    }
}

fn refresh_runner_usage_sync(runner_type: String) -> RunnerUsageSnapshot {
    if runner_type.trim().to_ascii_lowercase() == "codex" {
        fetch_codex_usage_via_http()
    } else {
        fetch_claude_usage()
    }
}

#[tauri::command]
pub async fn refresh_runner_usage(runner_type: String) -> Result<RunnerUsageSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || refresh_runner_usage_sync(runner_type))
        .await
        .map_err(|error| format!("usage refresh task failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::parse_rfc3339_ms;

    #[test]
    fn parses_epoch() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn parses_fractional_seconds_and_zero_offset() {
        // Shape returned by the live OAuth usage endpoint.
        assert_eq!(
            parse_rfc3339_ms("2026-09-12T20:50:00.364745+00:00"),
            Some(1_789_246_200_000)
        );
    }

    #[test]
    fn applies_non_zero_offset() {
        let utc = parse_rfc3339_ms("2026-09-12T20:50:00Z").unwrap();
        // +02:00 means the same wall clock is two hours earlier in UTC.
        assert_eq!(
            parse_rfc3339_ms("2026-09-12T22:50:00+02:00"),
            Some(utc)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-09-12T18:50:00-02:00"),
            Some(utc)
        );
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_rfc3339_ms("not a timestamp"), None);
        assert_eq!(parse_rfc3339_ms(""), None);
    }
}
