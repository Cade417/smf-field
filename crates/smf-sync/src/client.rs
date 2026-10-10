//! HTTP client for the Show Me Fire Field collaboration API (callback style, see `smf-net`).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use smf_net::{Method, Request};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum SyncError {
    /// 401: the token is missing, wrong or was revoked.
    Auth(String),
    /// Any other refusal, with the server's explanation (402 means the plan is not active).
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
    /// The team's plan and quotas (absent from older servers).
    #[serde(default)]
    pub plan: Option<PlanInfo>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct PlanLimits {
    #[serde(default)]
    pub max_members: u64,
    #[serde(default)]
    pub max_maps: u64,
    #[serde(default)]
    pub max_features: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct PlanUsage {
    #[serde(default)]
    pub members: u64,
    #[serde(default)]
    pub maps: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct PlanInfo {
    pub plan: String,
    /// "active", "expired" or "suspended"; when not active the team is read-only.
    pub status: String,
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub limits: PlanLimits,
    #[serde(default)]
    pub usage: PlanUsage,
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
}

impl std::fmt::Debug for FieldClient {
    // Never print the token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FieldClient({})", self.base)
    }
}

const API: &str = "/api/field/v1";

/// Turn a finished request into a JSON value or a classified error.
fn classify(r: Result<smf_net::Response, String>) -> Result<Value, SyncError> {
    let resp = r.map_err(SyncError::Network)?;
    let text = resp.text();
    if resp.ok() {
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
        .unwrap_or_else(|| format!("the server answered {}", resp.status));
    if resp.status == 401 {
        Err(SyncError::Auth(detail))
    } else {
        Err(SyncError::Server {
            status: resp.status,
            message: detail,
        })
    }
}

impl FieldClient {
    pub fn new(base: &str, token: Option<String>) -> Result<Self, SyncError> {
        let base = base.trim().trim_end_matches('/').to_string();
        if !(base.starts_with("https://") || base.starts_with("http://")) || base.len() < 9 {
            return Err(SyncError::Network(
                "the server address must start with http:// or https://".into(),
            ));
        }
        Ok(Self { base, token })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{API}{path}", self.base)
    }

    fn req(&self, method: Method, path: &str) -> Request {
        let r = Request::new(method, self.url(path)).timeout(Duration::from_secs(20));
        match &self.token {
            Some(t) => r.bearer(t),
            None => r,
        }
    }

    fn call<T: for<'de> Deserialize<'de> + Send + 'static>(
        &self,
        req: Request,
        done: impl FnOnce(Result<T, SyncError>) + Send + 'static,
    ) {
        smf_net::fetch(req, move |r| {
            done(classify(r).and_then(|v| {
                serde_json::from_value(v).map_err(|e| SyncError::Parse(e.to_string()))
            }))
        });
    }

    fn call_unit(&self, req: Request, done: impl FnOnce(Result<(), SyncError>) + Send + 'static) {
        smf_net::fetch(req, move |r| done(classify(r).map(|_| ())));
    }

    /// Create a team. `credential` is a Show Me Fire account token (`fldacct_...`), which
    /// applies the account's plan; anything else is treated as the server operator's key.
    pub fn create_team(
        &self,
        credential: &str,
        team: &str,
        display: &str,
        done: impl FnOnce(Result<Creds, SyncError>) + Send + 'static,
    ) {
        let credential = credential.trim();
        let mut req = Request::new(Method::Post, self.url("/teams"));
        req = if credential.starts_with("fldacct_") {
            req.bearer(credential)
        } else {
            req.header("X-Field-Create-Key", credential)
        };
        self.call(
            req.json_body(&json!({"team_name": team, "display_name": display}).to_string()),
            done,
        );
    }

    pub fn join(
        &self,
        code: &str,
        display: &str,
        done: impl FnOnce(Result<Creds, SyncError>) + Send + 'static,
    ) {
        let req = Request::new(Method::Post, self.url("/teams/join"))
            .json_body(&json!({"join_code": code, "display_name": display}).to_string());
        self.call(req, done);
    }

    pub fn me(&self, done: impl FnOnce(Result<Me, SyncError>) + Send + 'static) {
        self.call(self.req(Method::Get, "/me"), done);
    }

    pub fn list_maps(&self, done: impl FnOnce(Result<Vec<MapInfo>, SyncError>) + Send + 'static) {
        #[derive(Deserialize)]
        struct Maps {
            maps: Vec<MapInfo>,
        }
        self.call(
            self.req(Method::Get, "/maps"),
            move |r: Result<Maps, SyncError>| done(r.map(|m| m.maps)),
        );
    }

    pub fn create_map(
        &self,
        name: &str,
        features: &BTreeMap<String, Value>,
        done: impl FnOnce(Result<MapInfo, SyncError>) + Send + 'static,
    ) {
        let body = json!({"name": name, "features": features}).to_string();
        self.call(self.req(Method::Post, "/maps").json_body(&body), done);
    }

    pub fn get_map(&self, id: &str, done: impl FnOnce(Result<MapDoc, SyncError>) + Send + 'static) {
        self.call(self.req(Method::Get, &format!("/maps/{id}")), done);
    }

    /// Replies with the map's new revision.
    pub fn post_ops(
        &self,
        map_id: &str,
        ops: &[crate::LocalOp],
        done: impl FnOnce(Result<u64, SyncError>) + Send + 'static,
    ) {
        let body: Vec<Value> = ops.iter().map(crate::LocalOp::to_json).collect();
        let req = self
            .req(Method::Post, &format!("/maps/{map_id}/ops"))
            .json_body(&json!({"ops": body}).to_string());
        smf_net::fetch(req, move |r| {
            done(classify(r).and_then(|v| {
                v.get("rev")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| SyncError::Parse("no revision in reply".into()))
            }))
        });
    }

    /// Long poll: replies as soon as there is news, or empty after `wait_secs` (max 25).
    pub fn events(
        &self,
        map_id: Option<&str>,
        since: u64,
        pos_since: u64,
        wait_secs: u64,
        done: impl FnOnce(Result<Events, SyncError>) + Send + 'static,
    ) {
        let wait = wait_secs.min(25);
        let mut q: Vec<(&str, String)> = vec![
            ("since", since.to_string()),
            ("pos_since", pos_since.to_string()),
            ("wait", wait.to_string()),
        ];
        if let Some(m) = map_id {
            q.push(("map_id", m.to_string()));
        }
        let req = self
            .req(Method::Get, "/events")
            .query(&q)
            .timeout(Duration::from_secs(wait + 15));
        self.call(req, done);
    }

    pub fn post_position(
        &self,
        p: &Position,
        done: impl FnOnce(Result<(), SyncError>) + Send + 'static,
    ) {
        let body = serde_json::to_string(p).unwrap_or_default();
        self.call_unit(self.req(Method::Post, "/positions").json_body(&body), done);
    }

    pub fn clear_position(&self, done: impl FnOnce(Result<(), SyncError>) + Send + 'static) {
        self.call_unit(self.req(Method::Delete, "/positions"), done);
    }

    pub fn new_join_code(&self, done: impl FnOnce(Result<String, SyncError>) + Send + 'static) {
        smf_net::fetch(self.req(Method::Post, "/teams/join-code"), move |r| {
            done(classify(r).and_then(|v| {
                v.get("join_code")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| SyncError::Parse("no join code in reply".into()))
            }))
        });
    }

    pub fn remove_member(
        &self,
        id: &str,
        done: impl FnOnce(Result<(), SyncError>) + Send + 'static,
    ) {
        self.call_unit(self.req(Method::Delete, &format!("/members/{id}")), done);
    }
}

/// Run a callback-style call and wait for its result. For tests and tools; browsers cannot block.
#[cfg(not(target_arch = "wasm32"))]
pub fn wait_blocking<T: Send + 'static>(
    start: impl FnOnce(Box<dyn FnOnce(Result<T, SyncError>) + Send>),
) -> Result<T, SyncError> {
    let (tx, rx) = std::sync::mpsc::channel();
    start(Box::new(move |r| {
        let _ = tx.send(r);
    }));
    rx.recv()
        .unwrap_or_else(|_| Err(SyncError::Network("no reply".into())))
}
