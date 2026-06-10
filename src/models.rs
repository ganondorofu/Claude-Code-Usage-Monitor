use std::time::SystemTime;

#[derive(Clone, Debug, Default)]
pub struct UsageSection {
    pub percentage: f64,
    pub resets_at: Option<SystemTime>,
}

#[derive(Clone, Debug, Default)]
pub struct UsageData {
    pub session: UsageSection,
    pub weekly: UsageSection,
}

#[derive(Clone, Debug, Default)]
pub struct AppUsageData {
    pub claude_code: Option<UsageData>,
    pub codex: Option<UsageData>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenSource {
    Env,
    GhCli,
    HostsYml,
    AppsJson,
    SavedFile,
}

impl TokenSource {
    pub fn label(&self) -> &'static str {
        match self {
            TokenSource::Env      => "env",
            TokenSource::GhCli    => "gh cli",
            TokenSource::HostsYml => "gh hosts",
            TokenSource::AppsJson => "copilot ext",
            TokenSource::SavedFile => "token file",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CopilotData {
    pub plan_type: String,
    pub premium_used: Option<u32>,
    pub premium_limit: u32,
    pub billing_cycle_end: Option<SystemTime>,
    pub token_source: Option<TokenSource>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum ActiveView {
    #[default]
    Claude,
    Copilot,
}
