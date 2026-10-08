mod auth;
mod error;
mod fsutil;
mod locale;
mod manager;
mod model;
mod paths;
mod pricing;
mod process;
mod remote;
mod usage;

pub use error::{CasError, Result};
pub use locale::ui_is_chinese;
pub use manager::Cas;
pub use model::{
    AccountChoice, AccountRecord, AccountStatus, AccountStatusSnapshot, AccountTestResult,
    AuthIdentity, BrowserLogin, CurrentAccount, DeviceLogin, ProcessInfo, Registry,
    RemoveActiveResult, SwitchResult, UsageWindow,
};
pub use paths::CasPaths;
pub use pricing::{
    ModelPriceSummary, ModelRate, PRICING_AS_OF, PricingSummary, ResponsePrice, UnpricedGroup,
    rate_for_model,
};
pub use remote::initialize_http_client;
pub use usage::{
    ModelUsage, SessionUsage, TierUsage, TokenCounts, UsageRecord, UsageReport, UsageSource,
    UsageTimeRange, scan_codex_usage, scan_codex_usage_in_range,
};
