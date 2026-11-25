//! A recording stub homeserver for the Matrix-write tests.
//!
//! It answers the calls mm-core makes on the start, resume and end paths
//! (state PUTs, timeline sends, the bot-invite sequence, power levels,
//! account data) and records every request with its path, query and body, plus
//! the event id it handed back, so a test can tell which write a stored id
//! belongs to.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: Value,
    /// The event id this stub answered with, when it answered with one.
    pub event_id: Option<String>,
}

impl Recorded {
    /// For `PUT .../state/{type}/{state_key}`: `(type, state_key)`.
    pub fn state_target(&self) -> Option<(String, String)> {
        if self.method != "PUT" {
            return None;
        }
        let rest = self.path.split("/state/").nth(1)?;
        let mut parts = rest.splitn(2, '/');
        let t = parts.next()?.to_string();
        let key = parts.next().unwrap_or("").to_string();
        Some((t, key))
    }

    /// For `PUT .../send/{type}/{txn}`: the event type.
    pub fn send_type(&self) -> Option<String> {
        if self.method != "PUT" {
            return None;
        }
        let rest = self.path.split("/send/").nth(1)?;
        rest.split('/').next().map(str::to_string)
    }

    /// The `user_id` the appservice impersonated (the event's sender).
    pub fn as_user(&self) -> Option<String> {
        self.query.split('&').find_map(|kv| {
            kv.strip_prefix("user_id=")
                .map(|v| urlencoding::decode(v).unwrap().into_owned())
        })
    }
}

#[derive(Default)]
pub struct StubHomeserver {
    records: Mutex<Vec<Recorded>>,
    counter: AtomicUsize,
    /// Paths containing any of these substrings answer 500 (`fail_always`) …
    fail_always: Mutex<Vec<String>>,
    /// … or 500 the first `n` times (`fail_times`).
    fail_times: Mutex<HashMap<String, usize>>,
    /// `GET` answers by path suffix (account data, power levels, state).
    get_answers: Mutex<Vec<(String, StatusCode, Value)>>,
}

impl StubHomeserver {
    pub fn recorded(&self) -> Vec<Recorded> {
        self.records.lock().unwrap().clone()
    }

    /// Every state PUT for `event_type`, in order.
    pub fn state_puts(&self, event_type: &str) -> Vec<Recorded> {
        self.recorded()
            .into_iter()
            .filter(|r| r.state_target().is_some_and(|(t, _)| t == event_type))
            .collect()
    }

    /// Every timeline send of `event_type`, in order.
    pub fn sends(&self, event_type: &str) -> Vec<Recorded> {
        self.recorded()
            .into_iter()
            .filter(|r| r.send_type().as_deref() == Some(event_type))
            .collect()
    }

    /// Every Matrix write (state PUT or timeline send), in order.
    pub fn writes(&self) -> Vec<Recorded> {
        self.recorded()
            .into_iter()
            .filter(|r| r.state_target().is_some() || r.send_type().is_some())
            .collect()
    }

    pub fn fail_always(&self, path_part: &str) {
        self.fail_always.lock().unwrap().push(path_part.to_string());
    }

    pub fn fail_times(&self, path_part: &str, n: usize) {
        self.fail_times
            .lock()
            .unwrap()
            .insert(path_part.to_string(), n);
    }

    /// Answer `GET` requests whose path ends with `suffix`.
    pub fn answer_get(&self, suffix: &str, status: StatusCode, body: Value) {
        self.get_answers
            .lock()
            .unwrap()
            .push((suffix.to_string(), status, body));
    }

    fn should_fail(&self, path: &str) -> bool {
        if self
            .fail_always
            .lock()
            .unwrap()
            .iter()
            .any(|p| path.contains(p.as_str()))
        {
            return true;
        }
        let mut times = self.fail_times.lock().unwrap();
        for (part, left) in times.iter_mut() {
            if path.contains(part.as_str()) && *left > 0 {
                *left -= 1;
                return true;
            }
        }
        false
    }
}

async fn handle(State(stub): State<Arc<StubHomeserver>>, req: Request<Body>) -> Response {
    let method = req.method().to_string();
    // Decoded, so a test can match a room id however the caller encoded it.
    let path = urlencoding::decode(req.uri().path())
        .map(|p| p.into_owned())
        .unwrap_or_else(|_| req.uri().path().to_string());
    let query = req.uri().query().unwrap_or("").to_string();
    let bytes = axum::body::to_bytes(req.into_body(), 1_000_000)
        .await
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

    let mut rec = Recorded {
        method: method.clone(),
        path: path.clone(),
        query,
        body,
        event_id: None,
    };

    let response = if stub.should_fail(&path) && method == "PUT" {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "errcode": "M_UNKNOWN", "error": "stub failure" })),
        )
            .into_response()
    } else if method == "POST"
        && path.contains("/_synapse/admin/v1/users/")
        && path.ends_with("/login")
    {
        axum::Json(json!({ "access_token": "syn_user_tok" })).into_response()
    } else if method == "POST" && path.ends_with("/invite") {
        axum::Json(json!({})).into_response()
    } else if method == "POST" && path.contains("/join") {
        axum::Json(json!({ "room_id": "!stub:room" })).into_response()
    } else if method == "GET" {
        let answer = stub
            .get_answers
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(suffix, _, _)| path.ends_with(suffix.as_str()))
            .cloned();
        match answer {
            Some((_, status, body)) => (status, axum::Json(body)).into_response(),
            None if path.contains("/state/m.room.power_levels") => {
                axum::Json(json!({ "users": {} })).into_response()
            }
            None => (
                StatusCode::NOT_FOUND,
                axum::Json(json!({ "errcode": "M_NOT_FOUND", "error": "not found" })),
            )
                .into_response(),
        }
    } else if method == "PUT" && (path.contains("/state/") || path.contains("/send/")) {
        let n = stub.counter.fetch_add(1, Ordering::SeqCst);
        let kind = if path.contains("/state/") {
            "state"
        } else {
            "send"
        };
        let id = format!("${kind}-{n}:stub");
        rec.event_id = Some(id.clone());
        axum::Json(json!({ "event_id": id })).into_response()
    } else {
        axum::Json(json!({})).into_response()
    };

    stub.records.lock().unwrap().push(rec);
    response
}

/// Start a stub on an ephemeral port; returns it and its base URL.
pub async fn spawn() -> (Arc<StubHomeserver>, String) {
    let stub = Arc::new(StubHomeserver::default());
    let app = axum::Router::new()
        .fallback(handle)
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (stub, format!("http://{addr}"))
}
