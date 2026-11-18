//! `SlackSocket` against a mock Slack: a Web API on 127.0.0.1 and a Socket Mode WebSocket server
//! the test drives frame by frame.
//!
//! The Web API mock checks which token each method gets (the app token only for
//! `apps.connections.open`, which must be a POST) and implements history and replies with
//! Slack's paging and `oldest` semantics, so gap checks are tested against realistic answers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use fleet_feeds::{
    ConversationKind, FeedError, FeedEvent, FeedMessage, SlackSocket, Source, SourceStatus,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_websockets::{Message, ServerBuilder, WebSocketStream};

const USER_TOKEN: &str = "xoxp-user-token";
const APP_TOKEN: &str = "xapp-app-token";
const WAIT: Duration = Duration::from_secs(10);

type Conn = WebSocketStream<TcpStream>;

#[derive(Default)]
struct Mock {
    ws_addr: String,
    history: HashMap<String, Vec<Value>>,
    threads: HashMap<String, Vec<Value>>,
    users: HashMap<String, String>,
    conversations: HashMap<String, Value>,
    log: Vec<(String, HashMap<String, String>)>,
    opens: usize,
    open_error: Option<&'static str>,
    history_error: Option<&'static str>,
    /// History requests still to answer with HTTP 429 and `Retry-After: 1`.
    history_429: usize,
    /// `users.conversations` requests still to answer with an error.
    members_errors: usize,
    token_misuse: Vec<String>,
}

fn tsf(ts: &str) -> f64 {
    ts.parse().unwrap_or(0.0)
}

fn ts_of(m: &Value) -> String {
    m["ts"].as_str().unwrap_or_default().to_owned()
}

fn page(mut items: Vec<Value>, q: &HashMap<String, String>) -> Value {
    let limit: usize = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(100);
    let offset: usize = q
        .get("cursor")
        .and_then(|c| c.strip_prefix("off:"))
        .and_then(|o| o.parse().ok())
        .unwrap_or(0);
    let total = items.len();
    let end = (offset + limit).min(total);
    let has_more = end < total;
    let slice: Vec<Value> = items.drain(offset.min(total)..end).collect();
    json!({
        "ok": true,
        "messages": slice,
        "has_more": has_more,
        "response_metadata": { "next_cursor": if has_more { format!("off:{end}") } else { String::new() } },
    })
}

impl Mock {
    fn annotate(&self, mut m: Value) -> Value {
        let ts = ts_of(&m);
        if let Some(replies) = self.threads.get(&ts)
            && !replies.is_empty()
        {
            let latest = replies
                .iter()
                .map(ts_of)
                .max_by(|a, b| tsf(a).total_cmp(&tsf(b)));
            m["thread_ts"] = json!(ts);
            m["reply_count"] = json!(replies.len());
            m["latest_reply"] = json!(latest);
        }
        m
    }

    fn handle(&mut self, method: &str, q: &HashMap<String, String>) -> Value {
        match method {
            "apps.connections.open" => {
                if let Some(e) = self.open_error {
                    return json!({"ok": false, "error": e});
                }
                self.opens += 1;
                json!({"ok": true, "url": format!("ws://{}/link/?ticket=t{}", self.ws_addr, self.opens)})
            }
            "auth.test" => {
                json!({"ok": true, "url": "https://acme.slack.com/", "user_id": "UME"})
            }
            "conversations.history" => {
                let channel = q.get("channel").cloned().unwrap_or_default();
                let Some(msgs) = self.history.get(&channel) else {
                    return json!({"ok": false, "error": "channel_not_found"});
                };
                if let Some(e) = self.history_error {
                    return json!({"ok": false, "error": e});
                }
                let oldest = q.get("oldest").map(|o| tsf(o)).unwrap_or(0.0);
                let latest = q.get("latest").map(|l| tsf(l)).unwrap_or(f64::MAX);
                let mut items: Vec<Value> = msgs
                    .iter()
                    .filter(|m| tsf(&ts_of(m)) > oldest && tsf(&ts_of(m)) < latest)
                    .map(|m| self.annotate(m.clone()))
                    .collect();
                items.sort_by(|a, b| tsf(&ts_of(b)).total_cmp(&tsf(&ts_of(a))));
                page(items, q)
            }
            "conversations.replies" => {
                let root_ts = q.get("ts").cloned().unwrap_or_default();
                let channel = q.get("channel").cloned().unwrap_or_default();
                let Some(root) = self
                    .history
                    .get(&channel)
                    .and_then(|ms| ms.iter().find(|m| ts_of(m) == root_ts))
                    .cloned()
                else {
                    return json!({"ok": false, "error": "thread_not_found"});
                };
                let oldest = q.get("oldest").map(|o| tsf(o)).unwrap_or(0.0);
                let mut replies: Vec<Value> = self
                    .threads
                    .get(&root_ts)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|m| tsf(&ts_of(m)) > oldest)
                    .collect();
                replies.sort_by(|a, b| tsf(&ts_of(a)).total_cmp(&tsf(&ts_of(b))));
                let mut items = vec![self.annotate(root)];
                items.extend(replies);
                page(items, q)
            }
            "users.info" => {
                let id = q.get("user").cloned().unwrap_or_default();
                match self.users.get(&id) {
                    Some(name) => json!({
                        "ok": true,
                        "user": {"id": id, "name": "handle", "profile": {"display_name": name}}
                    }),
                    None => json!({"ok": false, "error": "user_not_found"}),
                }
            }
            "conversations.info" => {
                let id = q.get("channel").cloned().unwrap_or_default();
                match self.conversations.get(&id) {
                    Some(c) => json!({"ok": true, "channel": c}),
                    None => json!({"ok": false, "error": "channel_not_found"}),
                }
            }
            "users.conversations" => {
                if self.members_errors > 0 {
                    self.members_errors -= 1;
                    return json!({"ok": false, "error": "internal_error"});
                }
                let channels: Vec<Value> = self.conversations.values().cloned().collect();
                json!({"ok": true, "channels": channels, "response_metadata": {"next_cursor": ""}})
            }
            _ => json!({"ok": false, "error": "unknown_method"}),
        }
    }
}

async fn route(
    State(mock): State<Arc<Mutex<Mock>>>,
    Path(method): Path<String>,
    http_method: Method,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let mut m = mock.lock().unwrap();
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let want = if method == "apps.connections.open" {
        APP_TOKEN
    } else {
        USER_TOKEN
    };
    let leaked = q.values().any(|v| v.contains("xoxp") || v.contains("xapp"));
    let wrong_verb = method == "apps.connections.open" && http_method != Method::POST;
    if auth != format!("Bearer {want}") || leaked || wrong_verb {
        m.token_misuse.push(method.clone());
        return axum::Json(json!({"ok": false, "error": "invalid_auth"})).into_response();
    }
    m.log.push((method.clone(), q.clone()));
    if method == "conversations.history" && m.history_429 > 0 {
        m.history_429 -= 1;
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "1")],
            "rate limited",
        )
            .into_response();
    }
    let v = m.handle(&method, &q);
    axum::Json(v).into_response()
}

struct Slack {
    url: String,
    mock: Arc<Mutex<Mock>>,
    conns: mpsc::Receiver<Conn>,
    base: u64,
}

impl Slack {
    async fn start() -> Self {
        let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock = Arc::new(Mutex::new(Mock {
            ws_addr: ws.local_addr().unwrap().to_string(),
            ..Mock::default()
        }));
        {
            let mut m = mock.lock().unwrap();
            m.users.insert("U1".into(), "alice".into());
            m.users.insert("U2".into(), "bob".into());
            m.users.insert("UME".into(), "owner".into());
            m.conversations.insert(
                "D1".into(),
                json!({"id": "D1", "is_im": true, "user": "U1"}),
            );
            m.conversations.insert(
                "C1".into(),
                json!({"id": "C1", "name": "general", "is_channel": true}),
            );
            m.conversations.insert(
                "G1".into(),
                json!({"id": "G1", "name": "mpdm-alice--bob--owner-1", "is_mpim": true, "is_private": true}),
            );
        }
        let app = Router::new()
            .route("/api/{method}", any(route))
            .with_state(mock.clone());
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = http.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(http, app).await.unwrap();
        });
        let (tx, conns) = mpsc::channel(8);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = ws.accept().await else {
                    return;
                };
                if let Ok((_, conn)) = ServerBuilder::new().accept(stream).await
                    && tx.send(conn).await.is_err()
                {
                    return;
                }
            }
        });
        let base = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 3600;
        Self {
            url: format!("http://{addr}/api"),
            mock,
            conns,
            base,
        }
    }

    fn ts(&self, i: u64) -> String {
        format!("{}.{:06}", self.base + i, 100)
    }

    fn source(&self) -> SlackSocket {
        SlackSocket::new(USER_TOKEN, APP_TOKEN)
            .with_base_url(&self.url)
            .with_reconnect_backoff(Duration::from_millis(20), Duration::from_millis(200))
            .with_backfill_request_gap(Duration::ZERO)
            .with_checkpoint_interval(Duration::from_millis(50))
    }

    /// Starts `src` with a sink of `capacity` and returns the receiving end.
    fn run_with(
        &self,
        mut src: SlackSocket,
        capacity: usize,
    ) -> (
        mpsc::Receiver<FeedEvent>,
        tokio::task::JoinHandle<Result<(), FeedError>>,
    ) {
        let (tx, rx) = mpsc::channel(capacity);
        let handle = tokio::spawn(async move { src.run(tx).await });
        (rx, handle)
    }

    /// Starts `src` with a sink and returns the receiving end.
    fn run(
        &self,
        mut src: SlackSocket,
    ) -> (
        mpsc::Receiver<FeedEvent>,
        tokio::task::JoinHandle<Result<(), FeedError>>,
    ) {
        let (tx, rx) = mpsc::channel(256);
        let handle = tokio::spawn(async move { src.run(tx).await });
        (rx, handle)
    }

    async fn accept(&mut self) -> Conn {
        tokio::time::timeout(WAIT, self.conns.recv())
            .await
            .expect("the client did not connect")
            .unwrap()
    }

    fn requests(&self, method: &str) -> Vec<HashMap<String, String>> {
        self.mock
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, q)| q.clone())
            .collect()
    }

    fn assert_tokens_handled(&self) {
        let misuse = self.mock.lock().unwrap().token_misuse.clone();
        assert!(misuse.is_empty(), "wrong token or verb for {misuse:?}");
    }
}

async fn send(conn: &mut Conn, v: Value) {
    conn.send(Message::text(v.to_string())).await.unwrap();
}

/// The next text frame from the client (pings and pongs skipped).
async fn read_text(conn: &mut Conn) -> Value {
    loop {
        let msg = tokio::time::timeout(WAIT, conn.next())
            .await
            .expect("no frame from the client")
            .expect("connection closed")
            .unwrap();
        if let Some(t) = msg.as_text() {
            return serde_json::from_str(t).unwrap();
        }
    }
}

fn hello() -> Value {
    json!({
        "type": "hello", "num_connections": 1,
        "debug_info": {"host": "applink-1", "approximate_connection_time": 18060},
        "connection_info": {"app_id": "A1"}
    })
}

fn envelope(envelope_id: &str, event_id: &str, event: Value) -> Value {
    json!({
        "envelope_id": envelope_id,
        "type": "events_api",
        "accepts_response_payload": false,
        "retry_attempt": 0,
        "retry_reason": "",
        "payload": {
            "token": "x", "team_id": "T1", "api_app_id": "A1", "type": "event_callback",
            "event_id": event_id, "event_time": 1,
            "authorizations": [{"team_id": "T1", "user_id": "UME", "is_bot": false}],
            "event": event,
        }
    })
}

fn message(channel: &str, channel_type: &str, ts: &str, user: &str, text: &str) -> Value {
    json!({
        "type": "message", "channel": channel, "channel_type": channel_type,
        "user": user, "text": text, "ts": ts, "event_ts": ts
    })
}

/// The next message event, skipping checkpoints and status.
async fn next_message(rx: &mut mpsc::Receiver<FeedEvent>) -> FeedMessage {
    loop {
        match next_event(rx).await {
            FeedEvent::Message(m) => return m,
            FeedEvent::Checkpoint(_) | FeedEvent::Status(_) => continue,
            other => panic!("expected a message, got {other:?}"),
        }
    }
}

/// The next status event, skipping checkpoints.
async fn next_status(rx: &mut mpsc::Receiver<FeedEvent>) -> SourceStatus {
    loop {
        match next_event(rx).await {
            FeedEvent::Status(s) => return s,
            FeedEvent::Checkpoint(_) => continue,
            other => panic!("expected a status, got {other:?}"),
        }
    }
}

async fn next_event(rx: &mut mpsc::Receiver<FeedEvent>) -> FeedEvent {
    tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("no event from the source")
        .expect("source stopped")
}

async fn next_non_checkpoint(rx: &mut mpsc::Receiver<FeedEvent>) -> FeedEvent {
    loop {
        match next_event(rx).await {
            FeedEvent::Checkpoint(_) | FeedEvent::Status(_) => continue,
            e => return e,
        }
    }
}

#[tokio::test]
async fn events_are_acked_with_their_envelope_id_and_delivered() {
    let mut s = Slack::start().await;
    let (mut rx, _run) = s.run(s.source());
    let mut conn = s.accept().await;
    send(&mut conn, hello()).await;

    let ts = s.ts(1);
    send(
        &mut conn,
        envelope(
            "env-1",
            "Ev1",
            message("D1", "im", &ts, "U1", "hi <@UME>, got a minute?"),
        ),
    )
    .await;
    assert_eq!(read_text(&mut conn).await, json!({"envelope_id": "env-1"}));

    let m = next_message(&mut rx).await;
    assert_eq!(m.source, "slack");
    assert_eq!(m.conversation_id, "D1");
    assert_eq!(
        m.conversation_name.as_deref(),
        Some("alice"),
        "a DM is named after the partner"
    );
    assert_eq!(m.conversation_kind, ConversationKind::Direct);
    assert_eq!(m.id, ts);
    assert_eq!(m.author_id.as_deref(), Some("U1"));
    assert_eq!(m.author_name.as_deref(), Some("alice"));
    assert_eq!(m.text, "hi @owner, got a minute?");
    assert!(m.mentions_owner);
    assert!(!m.from_owner);
    assert_eq!(
        m.permalink.as_deref(),
        Some(
            format!(
                "https://acme.slack.com/archives/D1/p{}",
                ts.replace('.', "")
            )
            .as_str()
        )
    );

    // Slack retries an event it thinks was not acknowledged: a new envelope, the same event id.
    // It is acknowledged again but delivered only once.
    send(
        &mut conn,
        envelope(
            "env-2",
            "Ev1",
            message("D1", "im", &ts, "U1", "hi <@UME>, got a minute?"),
        ),
    )
    .await;
    assert_eq!(read_text(&mut conn).await, json!({"envelope_id": "env-2"}));

    // The owner's own message (a reply the fleet posted, say) and a group DM thread reply.
    let reply = json!({
        "type": "message", "channel": "G1", "channel_type": "mpim", "user": "UME",
        "text": "on it <@UME>", "ts": s.ts(3), "thread_ts": s.ts(2), "event_ts": s.ts(3)
    });
    send(&mut conn, envelope("env-3", "Ev3", reply)).await;
    assert_eq!(read_text(&mut conn).await, json!({"envelope_id": "env-3"}));
    let m = next_message(&mut rx).await;
    assert_eq!(
        m.text, "on it @owner",
        "the retried event must not come through"
    );
    assert!(m.from_owner);
    assert!(!m.mentions_owner, "the owner does not mention themselves");
    assert_eq!(m.conversation_kind, ConversationKind::Group);
    assert_eq!(m.conversation_name.as_deref(), Some("alice, bob, owner"));
    assert_eq!(m.thread_id, Some(s.ts(2)));

    // Progress is checkpointed, and the checkpoint restores.
    let cp = loop {
        if let FeedEvent::Checkpoint(c) = next_event(&mut rx).await {
            break c;
        }
    };
    assert_eq!(cp["conversations"]["D1"]["last_ts"], json!(ts));
    let mut again = s.source();
    again.restore_cursor(cp.clone());
    assert_eq!(again.cursor(), cp);
    s.assert_tokens_handled();
}

#[tokio::test]
async fn refreshes_hand_over_and_lost_connections_reconnect() {
    let mut s = Slack::start().await;
    s.mock
        .lock()
        .unwrap()
        .history
        .entry("C1".into())
        .or_default();
    let mut src = s.source();
    src.restore_cursor(json!({
        "mode": "slack_socket/1",
        "conversations": {"C1": {"last_ts": s.ts(0)}}
    }));
    let (mut rx, _run) = s.run(src);

    let mut first = s.accept().await;
    send(&mut first, hello()).await;
    assert_eq!(next_status(&mut rx).await, SourceStatus::Connected);
    // Let the first round of gap checks run, so that any later one would show.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let checks_before = s.requests("conversations.history").len();
    send(
        &mut first,
        json!({"type": "disconnect", "reason": "refresh_requested", "debug_info": {"host": "applink-1"}}),
    )
    .await;

    // The replacement is opened while the old connection is still read and acknowledged.
    let mut second = s.accept().await;
    assert_eq!(s.mock.lock().unwrap().opens, 2);
    send(
        &mut first,
        envelope(
            "env-0",
            "Ev0",
            message("C1", "channel", &s.ts(0), "U2", "during handover"),
        ),
    )
    .await;
    assert_eq!(read_text(&mut first).await["envelope_id"], "env-0");
    assert_eq!(next_message(&mut rx).await.text, "during handover");
    send(&mut second, hello()).await;
    send(
        &mut second,
        envelope(
            "env-1",
            "Ev1",
            message("C1", "channel", &s.ts(1), "U2", "after refresh"),
        ),
    )
    .await;
    assert_eq!(read_text(&mut second).await["envelope_id"], "env-1");
    let m = next_message(&mut rx).await;
    assert_eq!(m.text, "after refresh");
    assert_eq!(m.conversation_kind, ConversationKind::Channel);
    assert_eq!(m.conversation_name.as_deref(), Some("general"));
    assert_eq!(
        s.requests("conversations.history").len(),
        checks_before,
        "a handover needs no gap check"
    );

    // The server goes away without a word: that is reported, and a new connection opened.
    drop(second);
    match next_status(&mut rx).await {
        SourceStatus::Disconnected { .. } => {}
        other => panic!("expected a disconnect, got {other:?}"),
    }
    let mut third = s.accept().await;
    assert_eq!(s.mock.lock().unwrap().opens, 3);
    send(&mut third, hello()).await;
    send(
        &mut third,
        envelope(
            "env-2",
            "Ev2",
            message("C1", "channel", &s.ts(2), "U2", "after drop"),
        ),
    )
    .await;
    assert_eq!(read_text(&mut third).await["envelope_id"], "env-2");
    assert_eq!(next_message(&mut rx).await.text, "after drop");
    s.assert_tokens_handled();
}

#[tokio::test]
async fn gaps_are_backfilled_on_connect_without_duplicates() {
    let mut s = Slack::start().await;
    // Before the outage: C1 was seen up to ts(10). During it: a top-level message, and a reply
    // in a thread whose root predates the outage.
    let root = s.ts(5);
    {
        let mut m = s.mock.lock().unwrap();
        let h = m.history.entry("C1".into()).or_default();
        h.push(json!({"type": "message", "ts": root, "user": "U1", "text": "old root"}));
        h.push(json!({"type": "message", "ts": s.ts(10), "user": "U1", "text": "seen before"}));
        h.push(
            json!({"type": "message", "ts": s.ts(11), "user": "U2", "text": "missed top level"}),
        );
        h.push(json!({"type": "message", "subtype": "channel_join", "ts": s.ts(12), "user": "U2", "text": "joined"}));
        m.threads.entry(root.clone()).or_default().extend([
            json!({"type": "message", "ts": s.ts(6), "thread_ts": root, "user": "U2", "text": "old reply"}),
            json!({"type": "message", "ts": s.ts(13), "thread_ts": root, "user": "U2", "text": "missed reply <@UME>"}),
        ]);
    }
    let mut src = s.source();
    src.restore_cursor(json!({
        "mode": "slack_socket/1",
        "conversations": {"C1": {"last_ts": s.ts(10), "kind": "channel"}}
    }));
    let (mut rx, _run) = s.run(src);
    let mut conn = s.accept().await;
    send(&mut conn, hello()).await;

    let a = next_message(&mut rx).await;
    let b = next_message(&mut rx).await;
    assert_eq!(a.text, "missed top level");
    assert_eq!(a.thread_id, None);
    assert_eq!(b.text, "missed reply @owner");
    assert_eq!(b.thread_id, Some(root.clone()));
    assert!(b.mentions_owner);
    assert_eq!(b.conversation_kind, ConversationKind::Channel);

    // The gap closes in the checkpoint, and the watermark moves to the newest message.
    let cp = loop {
        if let FeedEvent::Checkpoint(c) = next_event(&mut rx).await
            && c["conversations"]["C1"].get("gap_from").is_none()
        {
            break c;
        }
    };
    assert_eq!(cp["conversations"]["C1"]["last_ts"], json!(s.ts(13)));

    // One history request reached back past the gap to see the thread root.
    let history = s.requests("conversations.history");
    assert_eq!(history.len(), 1);
    let oldest: f64 = history[0]["oldest"].parse().unwrap();
    assert!(
        oldest < tsf(&root),
        "the lookback must cover the thread root"
    );
    let replies = s.requests("conversations.replies");
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["oldest"], s.ts(10));

    // The same message arriving live (Slack delivers what was in flight) is not repeated.
    send(
        &mut conn,
        envelope(
            "env-1",
            "Ev1",
            message("C1", "channel", &s.ts(11), "U2", "missed top level"),
        ),
    )
    .await;
    send(
        &mut conn,
        envelope(
            "env-2",
            "Ev2",
            message("C1", "channel", &s.ts(14), "U2", "live"),
        ),
    )
    .await;
    assert_eq!(next_message(&mut rx).await.text, "live");
}

#[tokio::test]
async fn edits_deletes_and_housekeeping() {
    let mut s = Slack::start().await;
    let (mut rx, _run) = s.run(s.source());
    let mut conn = s.accept().await;
    send(&mut conn, hello()).await;
    let ts = s.ts(1);

    // An unfurl: `message_changed` with the text unchanged.
    let same = json!({
        "type": "message", "subtype": "message_changed", "hidden": true, "channel": "C1",
        "channel_type": "channel", "ts": s.ts(2), "event_ts": s.ts(2),
        "message": {"type": "message", "user": "U1", "text": "see https://x.io", "ts": ts},
        "previous_message": {"type": "message", "user": "U1", "text": "see https://x.io", "ts": ts}
    });
    send(&mut conn, envelope("e1", "Ev1", same)).await;
    send(
        &mut conn,
        envelope(
            "e2",
            "Ev2",
            json!({"type": "message", "subtype": "channel_join", "channel": "C1", "channel_type": "channel",
                   "user": "U2", "text": "<@U2> has joined", "ts": s.ts(3)}),
        ),
    )
    .await;
    // A real edit.
    let edit = json!({
        "type": "message", "subtype": "message_changed", "hidden": true, "channel": "C1",
        "channel_type": "channel", "ts": s.ts(4), "event_ts": s.ts(4),
        "message": {"type": "message", "user": "U1", "text": "see https://y.io instead", "ts": ts,
                    "edited": {"user": "U1", "ts": s.ts(4)}},
        "previous_message": {"type": "message", "user": "U1", "text": "see https://x.io", "ts": ts}
    });
    send(&mut conn, envelope("e3", "Ev3", edit)).await;
    let deleted = json!({
        "type": "message", "subtype": "message_deleted", "hidden": true, "channel": "C1",
        "channel_type": "channel", "ts": s.ts(5), "deleted_ts": ts, "event_ts": s.ts(5),
        "previous_message": {"type": "message", "user": "U1", "text": "see https://y.io instead", "ts": ts}
    });
    send(&mut conn, envelope("e4", "Ev4", deleted)).await;
    // Something other than a message event is acknowledged and ignored.
    send(
        &mut conn,
        envelope(
            "e5",
            "Ev5",
            json!({"type": "reaction_added", "user": "U1", "reaction": "eyes"}),
        ),
    )
    .await;
    for id in ["e1", "e2", "e3", "e4", "e5"] {
        assert_eq!(read_text(&mut conn).await["envelope_id"], id);
    }

    match next_non_checkpoint(&mut rx).await {
        FeedEvent::Message(m) => {
            assert_eq!(m.text, "see https://y.io instead");
            assert_eq!(m.id, ts, "an edit keeps the original id");
            assert!(m.edited);
        }
        other => panic!("expected the edit, got {other:?}"),
    }
    match next_non_checkpoint(&mut rx).await {
        FeedEvent::Deleted {
            source,
            conversation_id,
            message_id,
        } => {
            assert_eq!(source, "slack");
            assert_eq!(conversation_id, "C1");
            assert_eq!(message_id, ts);
        }
        other => panic!("expected the deletion, got {other:?}"),
    }
}

#[tokio::test]
async fn unusable_app_token_stops_run() {
    let s = Slack::start().await;
    s.mock.lock().unwrap().open_error = Some("not_allowed_token_type");
    let (_rx, run) = s.run(s.source());
    let res = tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
    assert_eq!(res, Err(FeedError::Api("not_allowed_token_type".into())));
}

#[tokio::test]
async fn explicit_backfill_and_directory_work_without_a_connection() {
    let s = Slack::start().await;
    {
        let mut m = s.mock.lock().unwrap();
        let h = m.history.entry("C1".into()).or_default();
        h.push(json!({"type": "message", "ts": s.ts(1), "user": "U1", "text": "one"}));
        h.push(json!({"type": "message", "ts": s.ts(2), "user": "U2", "text": "two"}));
    }
    let mut src = s.source();
    src.restore_cursor(json!({
        "mode": "slack_socket/1",
        "conversations": {"C1": {"last_ts": s.ts(2)}}
    }));
    let msgs = src.backfill(s.base as f64 + 0.5).await.unwrap();
    let texts: Vec<&str> = msgs.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["one", "two"]);
    assert_eq!(msgs[0].conversation_kind, ConversationKind::Channel);

    let caps = src.capabilities();
    assert!(caps.push && caps.backfill && caps.can_reply && !caps.poll);
    assert!(matches!(src.poll().await, Err(FeedError::Unsupported(_))));

    let dir = src.directory().unwrap();
    let d1 = dir.conversation("D1").await.unwrap();
    assert_eq!(d1.kind, ConversationKind::Direct);
    assert_eq!(d1.name.as_deref(), Some("alice"));
    assert_eq!(dir.author("U2").await.unwrap().name.as_deref(), Some("bob"));
    assert_eq!(dir.author("U404").await.unwrap().name, None);
    let mut all: Vec<String> = dir
        .conversations()
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    all.sort();
    assert_eq!(all, ["C1", "D1", "G1"]);
    // Cached: a second listing makes no request.
    let before = s.requests("users.conversations").len();
    dir.conversations().await.unwrap();
    assert_eq!(s.requests("users.conversations").len(), before);
    s.assert_tokens_handled();
}

#[tokio::test]
async fn a_polling_cursor_seeds_gap_checks() {
    let s = Slack::start().await;
    let mut src = s.source();
    src.restore_cursor(json!({
        "v": 1,
        "channels": {"C1": {"last_ts": s.ts(3), "last_sweep": 0.0}}
    }));
    assert_eq!(
        src.cursor()["conversations"]["C1"]["last_ts"],
        json!(s.ts(3))
    );
}

/// The next checkpoint in which `channel` has no open gap.
async fn gap_closed(rx: &mut mpsc::Receiver<FeedEvent>, channel: &str) -> Value {
    loop {
        if let FeedEvent::Checkpoint(c) = next_event(rx).await
            && c["conversations"][channel].get("gap_from").is_none()
        {
            return c;
        }
    }
}

#[tokio::test]
async fn a_long_outage_checks_every_member_conversation() {
    let mut s = Slack::start().await;
    // A direct message nobody had seen before arrived during the outage.
    {
        let mut m = s.mock.lock().unwrap();
        m.history.entry("C1".into()).or_default();
        m.history.entry("G1".into()).or_default();
        m.history.entry("D1".into()).or_default().push(
            json!({"type": "message", "ts": s.ts(5), "user": "U1", "text": "are you there?"}),
        );
    }
    let mut src = s.source();
    // The connection was lost at `base`, an hour ago; C1 was the only known conversation.
    src.restore_cursor(json!({
        "mode": "slack_socket/1",
        "last_alive": s.base as f64,
        "conversations": {"C1": {"last_ts": s.ts(0)}}
    }));
    let (mut rx, _run) = s.run(src);
    let mut conn = s.accept().await;
    send(&mut conn, hello()).await;

    let m = next_message(&mut rx).await;
    assert_eq!(m.text, "are you there?");
    assert_eq!(m.conversation_id, "D1");
    assert_eq!(m.conversation_kind, ConversationKind::Direct);
    let cp = gap_closed(&mut rx, "D1").await;
    // The DM now has a real mark; the empty ones were checked and let go again.
    assert_eq!(cp["conversations"]["D1"]["last_ts"], json!(s.ts(5)));
    let checked: Vec<String> = s
        .requests("conversations.history")
        .iter()
        .map(|q| q["channel"].clone())
        .collect();
    assert_eq!(
        checked,
        ["D1", "G1", "C1"],
        "direct and group messages first"
    );
}

#[tokio::test]
async fn a_large_gap_is_checked_in_steps_without_losing_the_oldest() {
    let mut s = Slack::start().await;
    {
        let mut m = s.mock.lock().unwrap();
        let h = m.history.entry("C1".into()).or_default();
        h.push(json!({"type": "message", "ts": s.ts(10), "user": "U1", "text": "seen"}));
        for i in 11..=15 {
            h.push(
                json!({"type": "message", "ts": s.ts(i), "user": "U1", "text": format!("m{i}")}),
            );
        }
    }
    let mut src = s.source().with_backfill_pages(1, 2);
    src.restore_cursor(json!({
        "mode": "slack_socket/1",
        "conversations": {"C1": {"last_ts": s.ts(10)}}
    }));
    let (mut rx, _run) = s.run(src);
    let mut conn = s.accept().await;
    send(&mut conn, hello()).await;

    let mut got = Vec::new();
    for _ in 0..5 {
        got.push(next_message(&mut rx).await.text);
    }
    got.sort();
    assert_eq!(got, ["m11", "m12", "m13", "m14", "m15"]);
    let cp = gap_closed(&mut rx, "C1").await;
    assert_eq!(cp["conversations"]["C1"]["last_ts"], json!(s.ts(15)));
    // Each later step continued below the oldest message of the one before.
    let latest: Vec<Option<String>> = s
        .requests("conversations.history")
        .iter()
        .map(|q| q.get("latest").cloned())
        .collect();
    assert_eq!(latest, [None, Some(s.ts(14)), Some(s.ts(12))]);
}

#[tokio::test]
async fn a_revoked_user_token_found_while_filling_gaps_ends_run() {
    let mut s = Slack::start().await;
    {
        let mut m = s.mock.lock().unwrap();
        m.history.entry("C1".into()).or_default();
        m.history_error = Some("token_revoked");
    }
    let mut src = s.source();
    src.restore_cursor(json!({
        "mode": "slack_socket/1",
        "conversations": {"C1": {"last_ts": s.ts(1)}}
    }));
    let (_rx, run) = s.run(src);
    let mut conn = s.accept().await;
    send(&mut conn, hello()).await;
    let res = tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
    assert_eq!(res, Err(FeedError::Api("token_revoked".into())));
}

#[tokio::test]
async fn backfill_runs_alongside_run_through_a_handle() {
    let mut s = Slack::start().await;
    s.mock
        .lock()
        .unwrap()
        .history
        .entry("C1".into())
        .or_default()
        .push(json!({"type": "message", "ts": s.ts(2), "user": "U1", "text": "two"}));
    let mut src = s.source();
    src.restore_cursor(json!({
        "mode": "slack_socket/1",
        "conversations": {"C1": {"last_ts": s.ts(2)}}
    }));
    let backfiller = src.backfiller().unwrap();
    let (_rx, _run) = s.run(src);
    let _conn = s.accept().await;
    let msgs = backfiller.backfill(s.base as f64).await.unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text, "two");
}

/// Adds top-level messages to a conversation's history in the mock.
fn history(s: &Slack, channel: &str, msgs: &[(u64, &str)]) {
    let mut m = s.mock.lock().unwrap();
    let h = m.history.entry(channel.into()).or_default();
    for (i, text) in msgs {
        h.push(json!({"type": "message", "ts": s.ts(*i), "user": "U1", "text": text}));
    }
}

/// Messages until a checkpoint shows none of `channels` with an open gap.
async fn messages_until_closed(
    rx: &mut mpsc::Receiver<FeedEvent>,
    channels: &[&str],
) -> Vec<String> {
    let mut got = Vec::new();
    loop {
        match next_event(rx).await {
            FeedEvent::Message(m) => got.push(m.text),
            FeedEvent::Checkpoint(c)
                if channels
                    .iter()
                    .all(|ch| c["conversations"][*ch].get("gap_from").is_none()) =>
            {
                return got;
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn acks_continue_while_the_consumer_is_stuck_and_overflow_is_recovered() {
    let mut s = Slack::start().await;
    history(
        &s,
        "C1",
        &[(1, "m1"), (2, "m2"), (3, "m3"), (4, "m4"), (5, "m5")],
    );
    // One event may wait for the consumer, and the consumer takes one and then stops reading.
    let (mut rx, _run) = s.run_with(s.source().with_event_queue(1), 1);
    let mut conn = s.accept().await;
    send(&mut conn, hello()).await;
    for i in 1..=5u64 {
        let ev = message("C1", "channel", &s.ts(i), "U1", &format!("m{i}"));
        send(
            &mut conn,
            envelope(&format!("env-{i}"), &format!("Ev{i}"), ev),
        )
        .await;
    }
    // Every envelope is acknowledged although nothing is being consumed.
    for i in 1..=5u64 {
        assert_eq!(
            read_text(&mut conn).await["envelope_id"],
            format!("env-{i}")
        );
    }
    // What did not fit was recovered from history: every message exactly once.
    let mut got = messages_until_closed(&mut rx, &["C1"]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(e) = rx.try_recv() {
        if let FeedEvent::Message(m) = e {
            got.push(m.text);
        }
    }
    got.sort();
    assert_eq!(got, ["m1", "m2", "m3", "m4", "m5"]);
    assert!(!s.requests("conversations.history").is_empty());
}

#[tokio::test]
async fn a_hello_during_a_gap_check_loses_and_repeats_nothing() {
    let mut s = Slack::start().await;
    history(&s, "C1", &[(1, "seen 1"), (2, "missed 1")]);
    history(&s, "C2", &[(1, "seen 2"), (3, "missed 2")]);
    let mut src = s
        .source()
        .with_backfill_request_gap(Duration::from_millis(150));
    src.restore_cursor(json!({
        "mode": "slack_socket/1",
        "conversations": {"C1": {"last_ts": s.ts(1)}, "C2": {"last_ts": s.ts(1)}}
    }));
    let (mut rx, _run) = s.run(src);
    let mut conn = s.accept().await;
    send(&mut conn, hello()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Slack would not do this on one connection, but it is the same as a reconnect arriving
    // while the first round is still running.
    send(&mut conn, hello()).await;
    let mut got = messages_until_closed(&mut rx, &["C1", "C2"]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    while let Ok(e) = rx.try_recv() {
        if let FeedEvent::Message(m) = e {
            got.push(m.text);
        }
    }
    got.sort();
    assert_eq!(got, ["missed 1", "missed 2"]);
}

#[tokio::test]
async fn threads_beyond_a_steps_budget_are_fetched_in_the_next_step() {
    let mut s = Slack::start().await;
    let (r1, r2) = (s.ts(5), s.ts(6));
    history(&s, "C1", &[(5, "root 1"), (6, "root 2"), (10, "seen")]);
    {
        let mut m = s.mock.lock().unwrap();
        m.threads.insert(
            r1.clone(),
            vec![json!({"type": "message", "ts": s.ts(11), "thread_ts": r1, "user": "U2", "text": "reply 1"})],
        );
        m.threads.insert(
            r2.clone(),
            vec![json!({"type": "message", "ts": s.ts(12), "thread_ts": r2, "user": "U2", "text": "reply 2"})],
        );
    }
    let mut src = s.source().with_max_backfill_threads(1);
    src.restore_cursor(json!({
        "mode": "slack_socket/1",
        "conversations": {"C1": {"last_ts": s.ts(10)}}
    }));
    let (mut rx, _run) = s.run(src);
    let mut conn = s.accept().await;
    send(&mut conn, hello()).await;
    let mut got = messages_until_closed(&mut rx, &["C1"]).await;
    got.sort();
    assert_eq!(got, ["reply 1", "reply 2"]);
    assert_eq!(s.requests("conversations.history").len(), 1);
    let roots: Vec<String> = s
        .requests("conversations.replies")
        .iter()
        .map(|q| q["ts"].clone())
        .collect();
    assert_eq!(
        roots,
        [r2, r1],
        "the most recent thread first, the other one step later"
    );
}

#[tokio::test]
async fn a_rate_limited_gap_check_waits_and_carries_on() {
    let mut s = Slack::start().await;
    history(&s, "C1", &[(1, "seen"), (2, "missed")]);
    s.mock.lock().unwrap().history_429 = 1;
    let mut src = s.source();
    src.restore_cursor(json!({
        "mode": "slack_socket/1",
        "conversations": {"C1": {"last_ts": s.ts(1)}}
    }));
    let (mut rx, _run) = s.run(src);
    let mut conn = s.accept().await;
    send(&mut conn, hello()).await;
    let started = std::time::Instant::now();
    assert_eq!(messages_until_closed(&mut rx, &["C1"]).await, ["missed"]);
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "Retry-After was honoured"
    );
    assert_eq!(s.requests("conversations.history").len(), 2);
}

#[tokio::test]
async fn a_failed_member_list_is_retried_and_still_seeds() {
    let mut s = Slack::start().await;
    history(&s, "C1", &[]);
    history(&s, "G1", &[]);
    history(&s, "D1", &[(5, "while you were away")]);
    s.mock.lock().unwrap().members_errors = 1;
    let mut src = s
        .source()
        .with_refill_after(Duration::from_millis(100))
        .with_keepalive(Duration::from_millis(50), Duration::from_secs(5));
    src.restore_cursor(json!({
        "mode": "slack_socket/1",
        "last_alive": s.base as f64,
        "conversations": {}
    }));
    let (mut rx, _run) = s.run(src);
    let mut conn = s.accept().await;
    send(&mut conn, hello()).await;
    let m = next_message(&mut rx).await;
    assert_eq!(m.text, "while you were away");
    assert_eq!(s.requests("users.conversations").len(), 2);
}

#[tokio::test]
async fn a_polling_cursor_gets_its_gap_checked_after_the_switch() {
    let mut s = Slack::start().await;
    history(&s, "C1", &[(1, "polled"), (2, "after the switch")]);
    let mut src = s.source();
    src.restore_cursor(json!({
        "v": 1,
        "channels": {"C1": {"last_ts": s.ts(1), "last_sweep": 0.0}}
    }));
    let (mut rx, _run) = s.run(src);
    let mut conn = s.accept().await;
    send(&mut conn, hello()).await;
    assert_eq!(
        messages_until_closed(&mut rx, &["C1"]).await,
        ["after the switch"]
    );
}

#[tokio::test]
async fn a_dm_shaped_like_slacks_user_scoped_event_is_delivered() {
    let mut s = Slack::start().await;
    let (mut rx, _run) = s.run(s.source());
    let mut conn = s.accept().await;
    send(&mut conn, hello()).await;
    let ts = s.ts(1);
    // The full envelope Slack sends for `message.im` on a user-token subscription, with the
    // fields a minimal event leaves out.
    let envelope = json!({
        "envelope_id": "8f3c5d1e-0000-4000-8000-000000000001",
        "type": "events_api",
        "accepts_response_payload": false,
        "retry_attempt": 0,
        "retry_reason": "",
        "payload": {
            "token": "x", "team_id": "T1", "context_team_id": "T1", "context_enterprise_id": null,
            "api_app_id": "A1", "type": "event_callback", "event_id": "Ev0001",
            "event_time": 1_700_000_000u64, "is_ext_shared_channel": false,
            "event_context": "4-eyJldCI6Im1lc3NhZ2UifQ",
            "authorizations": [{"enterprise_id": null, "team_id": "T1", "user_id": "UME",
                                "is_bot": false, "is_enterprise_install": false}],
            "event": {
                "user": "U1", "type": "message", "ts": ts, "client_msg_id": "c0ffee",
                "text": "can you look at this?", "team": "T1",
                "blocks": [{"type": "rich_text", "block_id": "b1", "elements": [
                    {"type": "rich_text_section", "elements": [{"type": "text", "text": "can you look at this?"}]}
                ]}],
                "channel": "D1", "event_ts": ts, "channel_type": "im"
            }
        }
    });
    send(&mut conn, envelope).await;
    assert_eq!(
        read_text(&mut conn).await["envelope_id"],
        "8f3c5d1e-0000-4000-8000-000000000001"
    );
    let m = next_message(&mut rx).await;
    assert_eq!(m.text, "can you look at this?");
    assert_eq!(m.conversation_kind, ConversationKind::Direct);
    assert!(!m.from_owner);
}

#[tokio::test]
async fn a_short_outage_still_checks_every_direct_message() {
    let mut s = Slack::start().await;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let days_ago = format!("{}.000100", now - 3 * 86_400);
    let during = format!("{}.000100", now - 2);
    {
        let mut m = s.mock.lock().unwrap();
        m.history.entry("D1".into()).or_default().extend([
            json!({"type": "message", "ts": days_ago, "user": "U1", "text": "last week"}),
            json!({"type": "message", "ts": during, "user": "U1", "text": "during the blip"}),
        ]);
        m.history.entry("G1".into()).or_default();
        m.history.entry("C1".into()).or_default().extend([
            json!({"type": "message", "ts": days_ago, "user": "U1", "text": "old channel talk"}),
            json!({"type": "message", "ts": during, "user": "U1", "text": "channel blip"}),
        ]);
    }
    let mut src = s.source();
    // Connected until five seconds ago; both conversations quiet for days before that.
    src.restore_cursor(json!({
        "mode": "slack_socket/1",
        "last_alive": (now - 5) as f64,
        "conversations": {
            "D1": {"last_ts": days_ago, "kind": "direct"},
            "C1": {"last_ts": days_ago, "kind": "channel"}
        }
    }));
    let (mut rx, _run) = s.run(src);
    let mut conn = s.accept().await;
    send(&mut conn, hello()).await;
    let m = next_message(&mut rx).await;
    assert_eq!(m.text, "during the blip");
    assert_eq!(m.conversation_kind, ConversationKind::Direct);
    messages_until_closed(&mut rx, &["D1", "G1"]).await;
    let checked: Vec<String> = s
        .requests("conversations.history")
        .iter()
        .map(|q| q["channel"].clone())
        .collect();
    assert_eq!(
        checked,
        ["D1", "G1"],
        "direct and group messages only; a quiet channel keeps the short window"
    );
    assert_eq!(s.requests("users.conversations").len(), 1);
}
