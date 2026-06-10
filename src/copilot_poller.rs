use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::models::{CopilotData, TokenSource};

const CREATE_NO_WINDOW: u32 = 0x08000000;
// VS Code Copilot extension uses these internal endpoints (not the org-only REST API)
const COPILOT_USER_URL: &str = "https://api.github.com/copilot_internal/user";

#[derive(Debug)]
pub enum CopilotPollError {
    NoCredentials,
    NoPlan,
    RequestFailed,
}

pub fn saved_token_path() -> Option<PathBuf> {
    std::env::var("APPDATA").ok().map(|d| {
        PathBuf::from(d)
            .join("ClaudeCodeUsageMonitor")
            .join("github_token.txt")
    })
}

fn try_saved_token() -> Option<String> {
    let content = std::fs::read_to_string(saved_token_path()?).ok()?;
    content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
}

pub fn poll() -> Result<CopilotData, CopilotPollError> {
    let (token, source) = get_github_token().ok_or(CopilotPollError::NoCredentials)?;

    let tls = native_tls::TlsConnector::new().map_err(|_| CopilotPollError::RequestFailed)?;
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .tls_connector(std::sync::Arc::new(tls))
        .build();

    let user_json = fetch_copilot_user_info(&agent, &token)?;

    // copilot_plan: "free" | "individual" | "individual_pro" | "business" | "enterprise"
    let plan_type = user_json["copilot_plan"]
        .as_str()
        .filter(|s| !s.is_empty() && *s != "null" && *s != "none" && *s != "not_found")
        .ok_or(CopilotPollError::NoPlan)?
        .to_string();

    let (premium_used, premium_limit, billing_cycle_end) = parse_quota(&user_json);

    Ok(CopilotData {
        plan_type,
        premium_used,
        premium_limit,
        billing_cycle_end,
        token_source: Some(source),
    })
}

fn fetch_copilot_user_info(agent: &ureq::Agent, token: &str) -> Result<Value, CopilotPollError> {
    // Use "token" prefix (not "Bearer") - same as VS Code Copilot extension
    let resp = agent
        .get(COPILOT_USER_URL)
        .set("Authorization", &format!("token {token}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2025-04-01")
        .set("User-Agent", &format!("claude-code-usage-monitor/{}", env!("CARGO_PKG_VERSION")))
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(401, _) | ureq::Error::Status(403, _) => CopilotPollError::NoCredentials,
            ureq::Error::Status(404, _) => CopilotPollError::NoPlan,
            _ => CopilotPollError::RequestFailed,
        })?;

    resp.into_json::<Value>().map_err(|_| CopilotPollError::RequestFailed)
}

/// Parse quota from quota_snapshots.premium_interactions (the "premium requests" counter).
/// Returns (used, limit, cycle_end).
fn parse_quota(json: &Value) -> (Option<u32>, u32, Option<SystemTime>) {
    let pi = &json["quota_snapshots"]["premium_interactions"];
    let plan = json["copilot_plan"].as_str().unwrap_or("");
    let cycle_end = parse_quota_reset_date(json["quota_reset_date"].as_str());

    if pi.is_null() || pi.is_object() == false {
        return (None, plan_to_limit(plan), cycle_end.or_else(|| Some(next_first_of_month())));
    }

    let unlimited = pi["unlimited"].as_bool().unwrap_or(false);
    let entitlement = pi["entitlement"].as_u64().unwrap_or(0) as u32;
    let remaining = pi["remaining"].as_u64().unwrap_or(0) as u32;

    if unlimited || entitlement == 0 {
        return (None, plan_to_limit(plan), cycle_end.or_else(|| Some(next_first_of_month())));
    }

    let used = entitlement.saturating_sub(remaining);
    let limit = entitlement; // actual entitlement from API is more accurate than our hardcoded table
    (Some(used), limit, cycle_end.or_else(|| Some(next_first_of_month())))
}

fn parse_quota_reset_date(date_str: Option<&str>) -> Option<SystemTime> {
    // quota_reset_date is like "2025-04-01"
    let s = date_str?;
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 { return None; }
    let year: u64 = parts[0].parse().ok()?;
    let month: u64 = parts[1].parse().ok()?;
    let day: u64 = parts[2].parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(date_to_unix_secs(year, month, day)))
}

fn get_github_token() -> Option<(String, TokenSource)> {
    // 0. Manually saved token file (highest priority)
    if let Some(t) = try_saved_token() {
        return Some((t, TokenSource::SavedFile));
    }

    // 1. Environment variables
    for var in &["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(var) {
            let t = t.trim().to_string();
            if !t.is_empty() {
                return Some((t, TokenSource::Env));
            }
        }
    }

    // 2. gh CLI: `gh auth token`
    if let Some(t) = try_gh_auth_token() {
        return Some((t, TokenSource::GhCli));
    }

    // 3. gh CLI hosts.yml
    if let Some(t) = read_gh_hosts_yml() {
        return Some((t, TokenSource::HostsYml));
    }

    // 4. VS Code Copilot extension apps.json
    read_copilot_apps_json().map(|t| (t, TokenSource::AppsJson))
}

fn try_gh_auth_token() -> Option<String> {
    let output = Command::new("gh")
        .args(["auth", "token"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let token = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if token.is_empty() || token.to_lowercase().starts_with("error") {
        None
    } else {
        Some(token)
    }
}

fn read_gh_hosts_yml() -> Option<String> {
    let mut paths = Vec::new();

    // Windows: %APPDATA%\GitHub CLI\hosts.yml
    if let Some(data_dir) = dirs::data_dir() {
        paths.push(data_dir.join("GitHub CLI").join("hosts.yml"));
    }
    // Linux/Mac fallback
    if let Some(config_dir) = dirs::config_dir() {
        paths.push(config_dir.join("gh").join("hosts.yml"));
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".config").join("gh").join("hosts.yml"));
    }

    for path in paths {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Some(token) = extract_yml_oauth_token(&content) {
                return Some(token);
            }
        }
    }
    None
}

fn extract_yml_oauth_token(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("oauth_token:") {
            let token = val.trim().to_string();
            if !token.is_empty() && token != "null" {
                return Some(token);
            }
        }
    }
    None
}

fn read_copilot_apps_json() -> Option<String> {
    // VS Code extension: %LOCALAPPDATA%\github-copilot\apps.json
    let local_dir = dirs::data_local_dir()?;
    let path = local_dir.join("github-copilot").join("apps.json");

    let content = std::fs::read_to_string(&path).ok()?;
    let json: Value = serde_json::from_str(&content).ok()?;

    // Try "github.com" key
    if let Some(token) = json["github.com"]["oauth_token"].as_str() {
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }

    // Try any key in the object
    if let Some(obj) = json.as_object() {
        for (_, v) in obj {
            if let Some(token) = v["oauth_token"].as_str() {
                if !token.is_empty() {
                    return Some(token.to_string());
                }
            }
        }
    }

    None
}

fn plan_to_limit(plan: &str) -> u32 {
    match plan.to_lowercase().as_str() {
        "free" => 50,
        "individual" | "pro" => 300,
        "individual_pro" => 1500,
        "business" => 300,
        "enterprise" => 1000,
        _ => 300,
    }
}

pub fn plan_display(plan: &str) -> &'static str {
    match plan.to_lowercase().as_str() {
        "free" => "Free",
        "individual" | "pro" => "Pro",
        "individual_pro" => "Pro+",
        "business" => "Business",
        "enterprise" => "Enterprise",
        _ => "Copilot",
    }
}

pub fn format_rows(data: &CopilotData) -> (f64, String, String) {
    let cycle_end = data.billing_cycle_end.unwrap_or_else(next_first_of_month);
    let countdown = format_countdown(Some(cycle_end));

    let percent = if data.premium_limit > 0 {
        data.premium_used
            .map(|u| ((u as f64) / (data.premium_limit as f64) * 100.0).min(100.0))
            .unwrap_or(0.0)
    } else {
        0.0
    };

    // Row 1 text: "17% · 14d" (has usage) or "Active · 14d" (no usage data)
    let row1_text = if data.premium_used.is_some() {
        let pct_str = format!("{:.0}%", percent);
        if countdown.is_empty() {
            pct_str
        } else {
            format!("{} \u{00b7} {}", pct_str, countdown)
        }
    } else if countdown.is_empty() {
        "Active".to_string()
    } else {
        format!("Active \u{00b7} {}", countdown)
    };

    // Row 2: plan + source + usage count (e.g. "Pro · gh cli · 150/300")
    let plan_name = plan_display(&data.plan_type);
    let base = match &data.token_source {
        Some(src) => format!("{} \u{00b7} {}", plan_name, src.label()),
        None => plan_name.to_string(),
    };
    let row2 = match data.premium_used {
        Some(used) => format!("{} \u{00b7} {}/{}", base, used, data.premium_limit),
        None => base,
    };

    (percent, row1_text, row2)
}

fn format_countdown(resets_at: Option<SystemTime>) -> String {
    let reset = match resets_at {
        Some(t) => t,
        None => return String::new(),
    };

    let remaining = match reset.duration_since(SystemTime::now()) {
        Ok(d) => d,
        Err(_) => return "now".to_string(),
    };

    let total_secs = remaining.as_secs();
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    if total_days >= 1 {
        format!("{total_days}d")
    } else if total_mins > 61 {
        format!("{total_hours}h")
    } else if total_secs > 60 {
        format!("{total_mins}m")
    } else {
        format!("{total_secs}")
    }
}

fn next_first_of_month() -> SystemTime {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let (year, month) = current_year_month(now_secs);
    let (next_year, next_month) = if month == 12 { (year + 1, 1u64) } else { (year, month + 1) };

    UNIX_EPOCH + Duration::from_secs(date_to_unix_secs(next_year, next_month, 1))
}

fn current_year_month(now_secs: u64) -> (u64, u64) {
    let mut remaining = now_secs / 86400;
    let mut year = 1970u64;

    loop {
        let y_days = if is_leap(year) { 366 } else { 365 };
        if remaining < y_days {
            break;
        }
        remaining -= y_days;
        year += 1;
    }

    let mut month = 1u64;
    loop {
        let m_days = month_len(year, month);
        if remaining < m_days {
            break;
        }
        remaining -= m_days;
        month += 1;
    }

    (year, month)
}

fn date_to_unix_secs(year: u64, month: u64, day: u64) -> u64 {
    let mut days = 0u64;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    for m in 1..month {
        days += month_len(year, m);
    }
    days += day - 1;
    days * 86400
}

fn month_len(year: u64, month: u64) -> u64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 30,
    }
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}
