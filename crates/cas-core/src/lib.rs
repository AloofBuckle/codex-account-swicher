mod auth;
mod error;
mod fsutil;
mod locale;
mod manager;
mod model;
mod paths;
mod process;
mod remote;

pub use error::{CasError, Result};
pub use locale::ui_is_chinese;
pub use manager::Cas;
pub use model::{
    AccountChoice, AccountRecord, AccountStatus, AccountStatusSnapshot, AccountTestResult,
    AuthIdentity, BrowserLogin, CurrentAccount, DeviceLogin, ProcessInfo, Registry, SwitchResult,
    UsageWindow,
};
pub use paths::CasPaths;
pub use remote::initialize_http_client;
