//! Update checks are for the desktop app; the web app is always the latest version.

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Failed(String),
}

pub fn check(_url: &str) -> UpdateState {
    UpdateState::UpToDate
}
