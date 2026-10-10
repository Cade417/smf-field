//! HTTP client for the Show Me Fire Field collaboration API.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum SyncError {
    /// 401: the token is missing, wrong or was revoked.
    Auth(String),
    /// Any other refusal, with the server's explanation.
    Server {
        status: u16,
        message: String,
    },
    Network(String),
    Parse(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Auth(m) => write!(f, "{m}"),
            SyncError::Server { message, .. } => write!(f, "{message}"),
            SyncError::Network(m) => write!(f, "could not reach the team server ({m})"),
            SyncError::Parse(m) => write!(f, "unexpected reply from the team server ({m})"),
        }
    }
}

impl std::error::Error for SyncError {}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Creds {
    pub team_id: String,
    pub team_name: String,
    pub member_id: String,
    pub token: String,
    /// Only returned when a team is created.
    #[serde(default)]
    pub join_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Member {
    pub id: String,
    pub display_name: String,
    pub is_owner: bool,
    #[serde(default)]
    pub last_seen: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Me {
    pub member_id: String,
    pub display_name: String,
    pub is_owner: bool,
    pub team_id: String,
    pub team_name: String,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MapInfo {
    pub id: String,
    pub name: String,
    pub rev: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MapDoc {
    pub id: String,
    pub name: String,
    pub rev: u64,
    pub features: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RemoteOp {
    pub rev: u64,
    pub uid: String,
    pub op: String,
    #[serde(default)]
    pub feature: Option<Value>,
    pub by: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TeamPosition {
    pub member_id: String,
    pub display_name: String,
    pub lat: f64,
    pub lng: f64,
    #[serde(default)]
    pub accuracy: Option<f64>,
    #[serde(default)]
    pub speed: Option<f64>,
    #[serde(default)]
    pub course: Option<f64>,
    #[serde(default)]
    pub alt: Option<f64>,
    /// The member stopped sharing (or was removed): drop their marker.
    #[serde(default)]
    pub cleared: bool,
    pub seq: u64,
    pub ts: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Events {
    pub rev: u64,
    #[serde(default)]
    pub ops: Vec<RemoteOp>,
    /// The client's copy is too old (or ahead): reload the whole map.
    #[serde(default)]
    pub reset: bool,
    #[serde(default)]
    pub positions: Vec<TeamPosition>,
    #[serde(default)]
    pub pos_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Position {
    pub lat: f64,
    pub lng: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accuracy: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub course: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alt: Option<f64>,
}

#[derive(Clone)]
pub struct FieldClient {
    base: String,
    token: Option<String>,
    http: reqwest::blocking::Client,
}

impl std::fmt::Debug for FieldClient {
    // Never print the token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FieldClient({})", self.base)
    }
}

const API: &str = "/api/field/v1";

impl FieldClient {
    pub fn new(base: &str, token: Option<String>) -> Result<Self, SyncError> {
        let base = base.trim().trim_end_matches('/').to_string();
        if !(base.starts_with("https://") || base.starts_with("http://")) || base.len() < 9 {
            return Err(SyncError::Network(
                "the server address must start with http:// or https://".into(),
            ));
        }
        let http = reqwest::blocking::Client::builder()
            .user_agent(concat!("ShowMeFireField/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| SyncError::Network(e.to_string()))?;
        Ok(Self { base, token, http })
    }

    fn send(&self, req: reqwest::blocking::RequestBuilder) -> Result<Value, SyncError> {
        let req = match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        };
        let resp = req
            .send()
            .map_err(|e| SyncError::Network(e.without_url().to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .map_err(|e| SyncError::Network(e.without_url().to_string()))?;
        if (200..300).contains(&status) {
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text).map_err(|e| SyncError::Parse(e.to_string()));
        }
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("detail").map(|d| match d {
                    Value::String(s) => s.clone(),
                    // Validation errors come back as a list; the first message is enough.
                    other => other
                        .get(0)
                        .and_then(|e| e.get("msg"))
                        .and_then(Value::as_str)
                        .unwrap_or("invalid request")
                        .to_string(),
                })
            })
            .unwrap_or_else(|| format!("the server answered {status}"));
        if status == 401 {
            Err(SyncError::Auth(detail))
        } else {
            Err(SyncError::Server {
                status,
                message: detail,
            })
        }
    }

    fn parse<T: for<'de> Deserialize<'de>>(v: Value) -> Result<T, SyncError> {
        serde_json::from_value(v).map_err(|e| SyncError::Parse(e.to_string()))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{API}{path}", self.base)
    }

    /// Needs the operator's create key: team creation is off on servers without one.
    pub fn create_team(
        &self,
        create_key: &str,
        team: &str,
        display: &str,
    ) -> Result<Creds, SyncError> {
        let r = self
            .http
            .post(self.url("/teams"))
            .header("X-Field-Create-Key", create_key)
            .json(&json!({"team_name": team, "display_name": display}));
        Self::parse(self.send(r)?)
    }

    pub fn join(&self, code: &str, display: &str) -> Result<Creds, SyncError> {
        let r = self
            .http
            .post(self.url("/teams/join"))
            .json(&json!({"join_code": code, "display_name": display}));
        Self::parse(self.send(r)?)
    }

    pub fn me(&self) -> Result<Me, SyncError> {
        Self::parse(self.send(self.http.get(self.url("/me")))?)
    }

    pub fn list_maps(&self) -> Result<Vec<MapInfo>, SyncError> {
        let v = self.send(self.http.get(self.url("/maps")))?;
        Self::parse(v.get("maps").cloned().unwrap_or(Value::Null))
    }

    pub fn create_map(
        &self,
        name: &str,
        features: &BTreeMap<String, Value>,
    ) -> Result<MapInfo, SyncError> {
        let r = self
            .http
            .post(self.url("/maps"))
            .json(&json!({"name": name, "features": features}));
        Self::parse(self.send(r)?)
    }

    pub fn get_map(&self, id: &str) -> Result<MapDoc, SyncError> {
        Self::parse(self.send(self.http.get(self.url(&format!("/maps/{id}"))))?)
    }

    /// Returns the map's new revision.
    pub fn post_ops(&self, map_id: &str, ops: &[crate::LocalOp]) -> Result<u64, SyncError> {
        let body: Vec<Value> = ops.iter().map(crate::LocalOp::to_json).collect();
        let r = self
            .http
            .post(self.url(&format!("/maps/{map_id}/ops")))
            .json(&json!({"ops": body}));
        let v = self.send(r)?;
        v.get("rev")
            .and_then(Value::as_u64)
            .ok_or_else(|| SyncError::Parse("no revision in reply".into()))
    }

    /// Long poll: returns as soon as there is news, or empty after `wait_secs` (max 25).
    pub fn events(
        &self,
        map_id: Option<&str>,
        since: u64,
        pos_since: u64,
        wait_secs: u64,
    ) -> Result<Events, SyncError> {
        let mut q: Vec<(&str, String)> = vec![
            ("since", since.to_string()),
            ("pos_since", pos_since.to_string()),
            ("wait", wait_secs.min(25).to_string()),
        ];
        if let Some(m) = map_id {
            q.push(("map_id", m.to_string()));
        }
        let r = self
            .http
            .get(self.url("/events"))
            .query(&q)
            .timeout(Duration::from_secs(wait_secs.min(25) + 15));
        Self::parse(self.send(r)?)
    }

    pub fn post_position(&self, p: &Position) -> Result<(), SyncError> {
        self.send(self.http.post(self.url("/positions")).json(p))
            .map(|_| ())
    }

    pub fn clear_position(&self) -> Result<(), SyncError> {
        self.send(self.http.delete(self.url("/positions")))
            .map(|_| ())
    }

    pub fn new_join_code(&self) -> Result<String, SyncError> {
        let v = self.send(self.http.post(self.url("/teams/join-code")))?;
        v.get("join_code")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| SyncError::Parse("no join code in reply".into()))
    }

    pub fn remove_member(&self, id: &str) -> Result<(), SyncError> {
        self.send(self.http.delete(self.url(&format!("/members/{id}"))))
            .map(|_| ())
    }
}
