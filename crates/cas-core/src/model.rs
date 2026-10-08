use serde::{Deserialize, Serialize};
use std::net::TcpListener;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AuthIdentity {
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub user_id: Option<String>,
}

impl AuthIdentity {
    pub fn display_name(&self) -> String {
        self.email
            .clone()
            .or_else(|| self.account_id.clone())
            .or_else(|| self.user_id.clone())
            .unwrap_or_else(|| "Unknown Codex account".into())
    }

    pub fn has_stable_field(&self) -> bool {
        self.email.is_some() || self.account_id.is_some() || self.user_id.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountRecord {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activated_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<AccountStatusSnapshot>,
}

impl AccountRecord {
    pub fn display_name(&self) -> String {
        self.alias
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.email.clone())
            .or_else(|| self.account_id.clone())
            .unwrap_or_else(|| self.id.clone())
    }

    pub fn identity(&self) -> AuthIdentity {
        AuthIdentity {
            email: self.email.clone(),
            account_id: self.account_id.clone(),
            user_id: self.user_id.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    pub used_percent: i32,
    pub remaining_percent: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_duration_mins: Option<i64>,
}

impl UsageWindow {
    pub fn label(&self) -> &'static str {
        match self.window_duration_mins {
            Some(300) => "5h",
            Some(10_080) => "week",
            Some(43_200) => "30d",
            _ => "long",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountStatusSnapshot {
    pub checked_at: i64,
    pub valid: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_window: Option<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AccountStatus {
    pub account: AccountRecord,
    pub snapshot: AccountStatusSnapshot,
}

#[derive(Debug, Clone)]
pub struct AccountChoice {
    pub account: AccountRecord,
    pub token_last_refresh_millis: Option<i64>,
    pub auth_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AccountTestResult {
    pub account: AccountRecord,
    pub reasoning_effort: String,
    pub response: Option<String>,
    pub error: Option<String>,
    pub request_headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Registry {
    pub schema_version: u32,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_account_id: Option<String>,
    #[serde(default)]
    pub accounts: Vec<AccountRecord>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            schema_version: 1,
            updated_at: chrono::Utc::now().timestamp(),
            active_account_id: None,
            accounts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct DeviceLogin {
    pub verification_url: String,
    pub user_code: String,
    pub(crate) device_auth_id: String,
    pub(crate) interval_secs: u64,
}

#[derive(Debug)]
pub struct BrowserLogin {
    pub auth_url: String,
    pub(crate) redirect_uri: String,
    pub(crate) code_verifier: String,
    pub(crate) state: String,
    pub(crate) listener: TcpListener,
}

#[derive(Debug, Clone)]
pub struct SwitchResult {
    pub from: Option<AccountRecord>,
    pub to: AccountRecord,
    pub terminated_processes: Vec<ProcessInfo>,
}

#[derive(Debug, Clone)]
pub struct RemoveActiveResult {
    pub account: AccountRecord,
    pub terminated_processes: Vec<ProcessInfo>,
}

#[derive(Debug, Clone)]
pub struct CurrentAccount {
    pub account: Option<AccountRecord>,
    pub identity: Option<AuthIdentity>,
    pub managed: bool,
    /// Plan from the actual active auth.json token, falling back to the last
    /// saved account status only when the active credential lacks a plan.
    pub plan_type: Option<String>,
}
