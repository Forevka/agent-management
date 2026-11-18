//! `SlackSource` against a mock Slack Web API on 127.0.0.1.
//!
//! The mock implements just enough of Slack's real semantics to make the tests meaningful:
//! history newest first with an exclusive `oldest` and cursor pagination, replies oldest first
//! with the root always included, `reply_count`/`latest_reply` on thread roots, bearer-token
//! auth, and injectable faults (HTTP 429 with `Retry-After`, `ok:false` errors).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use fleet_feeds::{ConversationKind, FeedError, FeedEvent, FeedMessage, SlackSource, Source};
use serde_json::{Value, json};

const TOKEN: &str = "xoxp-test-token";

type Matcher = Box<dyn Fn(&HashMap<String, String>) -> bool + Send>;

enum FaultKind {
    /// HTTP 429 with `Retry-After`.
    TooMany(u64),
    /// HTTP 200 with `{"ok":false,"error":"ratelimited"}` and `Retry-After`.
    RatelimitedBody(u64),
    /// HTTP 200 with `{"ok":false,"error":…}`.
    ApiError(&'static str),
}

struct Fault {
    method: &'static str,
    when: Matcher,
    kind: FaultKind,
    times: usize,
}

#[derive(Clone, Debug)]
struct Req {
    method: String,
    query: HashMap<String, String>,
}

#[derive(Default)]
struct Mock {
    /// Channel id to its top-level messages (including thread broadcasts).
    history: HashMap<String, Vec<Value>>,
    /// Thread root ts to its replies. Tests use distinct timestamps, so the channel is implied.
    threads: HashMap<String, Vec<Value>>,
    users: HashMap<String, String>,
    channel_names: HashMap<String, String>,
    /// Channels the owner can read but is not in.
    non_members: HashSet<String>,
    faults: Vec<Fault>,
    log: Vec<Req>,
    posts: Vec<Value>,
    /// Requests that did not carry the right bearer token, or carried a token anywhere else.
    token_misuse: usize,
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

    fn handle(&mut self, method: &str, q: &HashMap<String, String>, body: &[u8]) -> Response {
        match method {
            "auth.test" => {
                Json(json!({"ok": true, "url": "https://acme.slack.com/", "user_id": "UME"}))
            }
            "conversations.history" => {
                let channel = q.get("channel").cloned().unwrap_or_default();
                let Some(msgs) = self.history.get(&channel) else {
                    return Json(json!({"ok": false, "error": "channel_not_found"}));
                };
                let oldest = q.get("oldest").map(|o| tsf(o));
                let inclusive = q.get("inclusive").map(String::as_str) == Some("true");
                let mut items: Vec<Value> = msgs
                    .iter()
                    .filter(|m| match oldest {
                        Some(o) if inclusive => tsf(&ts_of(m)) >= o,
                        Some(o) => tsf(&ts_of(m)) > o,
                        None => true,
                    })
                    .map(|m| self.annotate(m.clone()))
                    .collect();
                items.sort_by(|a, b| tsf(&ts_of(b)).total_cmp(&tsf(&ts_of(a))));
                Json(page(items, q))
            }
            "conversations.replies" => {
                let channel = q.get("channel").cloned().unwrap_or_default();
                let root_ts = q.get("ts").cloned().unwrap_or_default();
                let root = self
                    .history
                    .get(&channel)
                    .and_then(|ms| ms.iter().find(|m| ts_of(m) == root_ts))
                    .cloned();
                let Some(root) = root else {
                    return Json(json!({"ok": false, "error": "thread_not_found"}));
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
                Json(page(items, q))
            }
            "users.info" => {
                let id = q.get("user").cloned().unwrap_or_default();
                match self.users.get(&id) {
                    Some(name) => Json(json!({
                        "ok": true,
                        "user": {"id": id, "name": "handle", "profile": {"display_name": name, "real_name": "Real"}}
                    })),
                    None => Json(json!({"ok": false, "error": "user_not_found"})),
                }
            }
            "conversations.info" => {
                let id = q.get("channel").cloned().unwrap_or_default();
                match self.channel_names.get(&id) {
                    Some(name) => Json(
                        json!({"ok": true, "channel": {"id": id, "name": name, "is_channel": true, "is_member": !self.non_members.contains(&id)}}),
                    ),
                    None => Json(json!({"ok": false, "error": "channel_not_found"})),
                }
            }
            "chat.getPermalink" => {
                let channel = q.get("channel").cloned().unwrap_or_default();
                let ts = q.get("message_ts").cloned().unwrap_or_default();
                Json(json!({
                    "ok": true,
                    "channel": channel,
                    "permalink": format!("https://acme.slack.com/archives/{channel}/p{}", ts.replace('.', ""))
                }))
            }
            "chat.postMessage" => {
                let Ok(v) = serde_json::from_slice::<Value>(body) else {
                    return Json(json!({"ok": false, "error": "invalid_json"}));
                };
                self.posts.push(v.clone());
                let channel = v["channel"].as_str().unwrap_or_default().to_owned();
                if !self.history.contains_key(&channel) {
                    return Json(json!({"ok": false, "error": "not_in_channel"}));
                }
                Json(json!({"ok": true, "channel": channel, "ts": "1800000000.000001"}))
            }
            _ => Json(json!({"ok": false, "error": "unknown_method"})),
        }
    }
}

#[allow(non_snake_case)]
fn Json(v: Value) -> Response {
    axum::Json(v).into_response()
}

async fn route(
    State(mock): State<Arc<Mutex<Mock>>>,
    Path(method): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut m = mock.lock().unwrap();
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let token_in_query = q.values().any(|v| v.contains("xoxp"));
    if auth != Some(&format!("Bearer {TOKEN}")) || token_in_query {
        m.token_misuse += 1;
        return Json(json!({"ok": false, "error": "invalid_auth"}));
    }
    m.log.push(Req {
        method: method.clone(),
        query: q.clone(),
    });
    if let Some(i) = m
        .faults
        .iter()
        .position(|f| f.method == method && (f.when)(&q))
    {
        let fault = &mut m.faults[i];
        fault.times -= 1;
        let resp = match fault.kind {
            FaultKind::TooMany(secs) => (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, secs.to_string())],
                "rate limited",
            )
                .into_response(),
            FaultKind::RatelimitedBody(secs) => (
                [(header::RETRY_AFTER, secs.to_string())],
                axum::Json(json!({"ok": false, "error": "ratelimited"})),
            )
                .into_response(),
            FaultKind::ApiError(code) => Json(json!({"ok": false, "error": code})),
        };
        if m.faults[i].times == 0 {
            m.faults.remove(i);
        }
        return resp;
    }
    m.handle(&method, &q, &body)
}

struct Slack {
    url: String,
    mock: Arc<Mutex<Mock>>,
    base: u64,
}

impl Slack {
    async fn start() -> Self {
        let mock = Arc::new(Mutex::new(Mock::default()));
        {
            let mut m = mock.lock().unwrap();
            m.users.insert("U1".into(), "alice".into());
            m.users.insert("U2".into(), "bob".into());
            m.channel_names.insert("C1".into(), "general".into());
            m.channel_names.insert("C2".into(), "random".into());
        }
        let app = Router::new()
            .route("/api/{method}", any(route))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // Recent timestamps, so that thread roots fall inside the default 24 h window.
        let base = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 3600;
        Self {
            url: format!("http://{addr}/api"),
            mock,
            base,
        }
    }

    fn source(&self, channels: &[&str]) -> SlackSource {
        SlackSource::new(TOKEN, channels.iter().map(|c| c.to_string()).collect())
            .with_base_url(&self.url)
    }

    fn ts(&self, i: u64) -> String {
        format!("{}.{:06}", self.base + i, 100)
    }

    fn post(&self, channel: &str, i: u64, user: &str, text: &str) -> String {
        let ts = self.ts(i);
        self.post_raw(
            channel,
            json!({"type": "message", "ts": ts, "user": user, "text": text}),
        );
        ts
    }

    fn post_raw(&self, channel: &str, m: Value) {
        self.mock
            .lock()
            .unwrap()
            .history
            .entry(channel.into())
            .or_default()
            .push(m);
    }

    fn reply(&self, root: &str, i: u64, user: &str, text: &str) -> String {
        let ts = self.ts(i);
        self.mock
            .lock()
            .unwrap()
            .threads
            .entry(root.into())
            .or_default()
            .push(
                json!({"type": "message", "ts": ts, "thread_ts": root, "user": user, "text": text}),
            );
        ts
    }

    fn fault(&self, method: &'static str, when: Matcher, kind: FaultKind) {
        self.mock.lock().unwrap().faults.push(Fault {
            method,
            when,
            kind,
            times: 1,
        });
    }

    fn requests(&self, method: &str) -> Vec<Req> {
        self.mock
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|r| r.method == method)
            .cloned()
            .collect()
    }

    fn clear_log(&self) {
        self.mock.lock().unwrap().log.clear();
    }

    fn assert_token_handled(&self) {
        assert_eq!(
            self.mock.lock().unwrap().token_misuse,
            0,
            "token missing or leaked into query"
        );
    }
}

fn texts(msgs: &[FeedMessage]) -> Vec<&str> {
    msgs.iter().map(|m| m.text.as_str()).collect()
}

#[tokio::test]
async fn first_poll_returns_history_oldest_first() {
    let s = Slack::start().await;
    s.post("C1", 1, "U1", "one");
    s.post("C1", 2, "U2", "two");
    s.post("C1", 3, "U1", "three");
    let mut src = s.source(&["C1"]);

    let msgs = src.poll().await.unwrap();
    assert_eq!(texts(&msgs), ["one", "two", "three"]);
    let first = &msgs[0];
    assert_eq!(first.source, "slack");
    assert_eq!(first.conversation_id, "C1");
    assert_eq!(first.conversation_name.as_deref(), Some("general"));
    assert_eq!(first.id, s.ts(1));
    assert_eq!(first.thread_id, None);
    assert_eq!(first.author_id.as_deref(), Some("U1"));
    assert_eq!(first.author_name.as_deref(), Some("alice"));
    assert!(!first.edited);
    assert!((first.ts_unix - (s.base + 1) as f64 - 0.0001).abs() < 1e-6);
    let expected_link = format!(
        "https://acme.slack.com/archives/C1/p{}",
        s.ts(1).replace('.', "")
    );
    assert_eq!(first.permalink.as_deref(), Some(expected_link.as_str()));
    assert_eq!(src.self_user_id(), Some("UME"));
    s.assert_token_handled();
}

#[tokio::test]
async fn later_polls_return_only_new_messages_with_one_request_when_quiet() {
    let s = Slack::start().await;
    s.post("C1", 1, "U1", "old");
    let mut src = s.source(&["C1"]);
    assert_eq!(texts(&src.poll().await.unwrap()), ["old"]);

    s.post("C1", 2, "U1", "new a");
    s.post("C1", 3, "U2", "new b");
    let msgs = src.poll().await.unwrap();
    assert_eq!(texts(&msgs), ["new a", "new b"]);
    let history = s.requests("conversations.history");
    let last = history.last().unwrap();
    assert_eq!(last.query.get("oldest"), Some(&s.ts(1)));
    assert_eq!(
        last.query.get("inclusive").map(String::as_str),
        Some("false")
    );
    assert_eq!(last.query.get("limit").map(String::as_str), Some("200"));

    // Nothing new: exactly one request, and no name lookups (they are cached).
    s.clear_log();
    assert!(src.poll().await.unwrap().is_empty());
    let log = s.mock.lock().unwrap().log.clone();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].method, "conversations.history");
}

#[tokio::test]
async fn pagination_follows_next_cursor_and_keeps_order() {
    let s = Slack::start().await;
    s.post("C1", 1, "U1", "seed");
    let mut src = s.source(&["C1"]).with_page_size(2);
    src.poll().await.unwrap();

    for i in 2..=6 {
        s.post("C1", i, "U1", &format!("m{i}"));
    }
    s.clear_log();
    let msgs = src.poll().await.unwrap();
    assert_eq!(texts(&msgs), ["m2", "m3", "m4", "m5", "m6"]);
    let history = s.requests("conversations.history");
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].query.get("cursor"), None);
    assert_eq!(
        history[1].query.get("cursor").map(String::as_str),
        Some("off:2")
    );
    assert_eq!(
        history[2].query.get("cursor").map(String::as_str),
        Some("off:4")
    );
    assert!(src.poll().await.unwrap().is_empty());
}

#[tokio::test]
async fn thread_replies_come_from_conversations_replies() {
    let s = Slack::start().await;
    let root = s.post("C1", 1, "U1", "question?");
    let mut src = s.source(&["C1"]).with_thread_sweep_interval(Duration::ZERO);
    assert_eq!(texts(&src.poll().await.unwrap()), ["question?"]);

    s.reply(&root, 2, "U2", "answer 1");
    s.reply(&root, 3, "U1", "answer 2");
    let msgs = src.poll().await.unwrap();
    assert_eq!(texts(&msgs), ["answer 1", "answer 2"]);
    assert!(
        msgs.iter()
            .all(|m| m.thread_id.as_deref() == Some(root.as_str()))
    );
    assert_eq!(msgs[0].author_name.as_deref(), Some("bob"));
    assert!(
        msgs[0]
            .permalink
            .as_deref()
            .unwrap()
            .ends_with(&format!("?thread_ts={root}&cid=C1"))
    );
    let replies = s.requests("conversations.replies");
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].query.get("ts"), Some(&root));

    // A later reply is fetched after the last one seen, and the earlier ones are not repeated.
    s.reply(&root, 4, "U2", "answer 3");
    let msgs = src.poll().await.unwrap();
    assert_eq!(texts(&msgs), ["answer 3"]);
    let replies = s.requests("conversations.replies");
    assert_eq!(replies.last().unwrap().query.get("oldest"), Some(&s.ts(3)));

    // A quiet thread costs no replies request.
    s.clear_log();
    assert!(src.poll().await.unwrap().is_empty());
    assert!(s.requests("conversations.replies").is_empty());

    // A reply also sent to the channel arrives once, through history, marked as a reply.
    let b = s.reply(&root, 5, "U1", "broadcast");
    {
        let mut m = s.mock.lock().unwrap();
        if let Some(r) = m.threads.get_mut(&root).and_then(|r| r.last_mut()) {
            r["subtype"] = json!("thread_broadcast");
        }
        m.history.get_mut("C1").unwrap().push(json!({
            "type": "message", "subtype": "thread_broadcast", "ts": b, "thread_ts": root,
            "user": "U1", "text": "broadcast"
        }));
    }
    let msgs = src.poll().await.unwrap();
    assert_eq!(texts(&msgs), ["broadcast"]);
    assert_eq!(msgs[0].thread_id.as_deref(), Some(root.as_str()));
    assert!(src.poll().await.unwrap().is_empty());
}

#[tokio::test]
async fn new_root_with_replies_is_fetched_without_waiting_for_a_sweep() {
    let s = Slack::start().await;
    s.post("C1", 1, "U1", "seed");
    let mut src = s
        .source(&["C1"])
        .with_thread_sweep_interval(Duration::from_secs(3600));
    src.poll().await.unwrap();

    let root = s.post("C1", 2, "U1", "root");
    s.reply(&root, 3, "U2", "fast reply");
    let msgs = src.poll().await.unwrap();
    assert_eq!(texts(&msgs), ["root", "fast reply"]);
    assert_eq!(msgs[1].thread_id.as_deref(), Some(root.as_str()));
}

#[tokio::test]
async fn rate_limit_mid_pagination_holds_pages_and_skips_nothing() {
    let s = Slack::start().await;
    s.post("C1", 1, "U1", "seed");
    let mut src = s.source(&["C1"]).with_page_size(2);
    src.poll().await.unwrap();
    let before = src.cursor();

    for i in 2..=6 {
        s.post("C1", i, "U1", &format!("m{i}"));
    }
    // The second page is refused. The first page holds the newest messages, so returning it
    // and advancing the cursor would skip the older ones for good.
    s.fault(
        "conversations.history",
        Box::new(|q| q.contains_key("cursor")),
        FaultKind::TooMany(1),
    );
    match src.poll().await {
        Err(FeedError::RateLimited(d)) => assert_eq!(d, Duration::from_secs(1)),
        other => panic!("expected RateLimited, got {other:?}"),
    }
    assert_eq!(
        src.cursor(),
        before,
        "cursor moved past messages it did not return"
    );
    assert!(src.rate_limited_for().is_some());

    // While the limit lasts, poll answers without calling Slack.
    s.clear_log();
    assert!(matches!(src.poll().await, Err(FeedError::RateLimited(_))));
    assert!(s.mock.lock().unwrap().log.is_empty());

    tokio::time::sleep(Duration::from_millis(1100)).await;
    let msgs = src.poll().await.unwrap();
    assert_eq!(texts(&msgs), ["m2", "m3", "m4", "m5", "m6"]);
    // It resumed the held pagination instead of starting over.
    let history = s.requests("conversations.history");
    assert_eq!(
        history[0].query.get("cursor").map(String::as_str),
        Some("off:2")
    );
    assert!(src.poll().await.unwrap().is_empty());
}

#[tokio::test]
async fn rate_limit_across_channels_returns_each_message_exactly_once() {
    let s = Slack::start().await;
    s.post("C1", 1, "U1", "a0");
    s.post("C2", 2, "U1", "b0");
    let mut src = s.source(&["C1", "C2"]);
    assert_eq!(src.poll().await.unwrap().len(), 2);

    s.post("C1", 3, "U1", "a1");
    s.post("C2", 4, "U2", "b1");
    s.post("C1", 5, "U1", "a2");
    s.fault(
        "conversations.history",
        Box::new(|q| q.get("channel").map(String::as_str) == Some("C2")),
        FaultKind::TooMany(1),
    );
    s.fault(
        "conversations.history",
        Box::new(|q| q.get("channel").map(String::as_str) == Some("C1")),
        FaultKind::RatelimitedBody(1),
    );

    let mut got: Vec<FeedMessage> = Vec::new();
    let mut limited = 0;
    for _ in 0..10 {
        match src.poll().await {
            Ok(msgs) => got.extend(msgs),
            Err(FeedError::RateLimited(_)) => limited += 1,
            Err(e) => panic!("unexpected error {e:?}"),
        }
        if got.len() >= 3 {
            break;
        }
        if let Some(wait) = src.rate_limited_for() {
            tokio::time::sleep(wait + Duration::from_millis(50)).await;
        }
    }
    assert!(limited >= 1, "the faults should have been hit");
    let mut ids: Vec<&str> = got.iter().map(|m| m.text.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["a1", "a2", "b1"], "every message exactly once");
    let unique: HashSet<&str> = got.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(unique.len(), got.len());
    assert!(src.poll().await.unwrap().is_empty());
}

#[tokio::test]
async fn cursor_round_trip_prevents_duplicates() {
    let s = Slack::start().await;
    let root = s.post("C1", 1, "U1", "root");
    s.post("C2", 2, "U1", "other");
    let mut src = s
        .source(&["C1", "C2"])
        .with_thread_sweep_interval(Duration::ZERO);
    assert_eq!(src.poll().await.unwrap().len(), 2);
    s.reply(&root, 3, "U2", "r1");
    assert_eq!(texts(&src.poll().await.unwrap()), ["r1"]);

    // Persist as a string, as the orchestrator's database would.
    let saved = serde_json::to_string(&src.cursor()).unwrap();
    drop(src);

    s.post("C1", 4, "U1", "after restart");
    s.reply(&root, 5, "U2", "r2");
    let mut fresh = s
        .source(&["C1", "C2"])
        .with_thread_sweep_interval(Duration::ZERO);
    fresh.restore_cursor(serde_json::from_str(&saved).unwrap());
    let msgs = fresh.poll().await.unwrap();
    assert_eq!(texts(&msgs), ["after restart", "r2"]);
    assert!(fresh.poll().await.unwrap().is_empty());

    // Without the cursor, the same source would start over with a backfill.
    let mut naive = s.source(&["C1", "C2"]);
    assert!(naive.poll().await.unwrap().len() >= 3);

    // Garbage is ignored rather than trusted.
    let mut odd = s.source(&["C1"]);
    odd.restore_cursor(json!({"channels": "nope"}));
    assert!(!odd.poll().await.unwrap().is_empty());
}

#[tokio::test]
async fn mrkdwn_is_converted_to_plain_text() {
    let s = Slack::start().await;
    s.post(
        "C1",
        1,
        "U1",
        "hey <@U2>, see <#C9|deploys> and <https://example.com/a?b=1&amp;c=2|the doc> &lt;3 <!here> <@U404>",
    );
    let mut src = s.source(&["C1"]);
    let msgs = src.poll().await.unwrap();
    assert_eq!(
        msgs[0].text,
        "hey @bob, see #deploys and the doc (https://example.com/a?b=1&c=2) <3 @here @U404"
    );
    let looked_up: Vec<String> = s
        .requests("users.info")
        .iter()
        .filter_map(|r| r.query.get("user").cloned())
        .collect();
    assert!(looked_up.contains(&"U2".to_owned()));
}

#[tokio::test]
async fn noise_subtypes_are_skipped_and_kept_ones_are_shaped() {
    let s = Slack::start().await;
    s.post("C1", 1, "U1", "seed");
    let mut src = s.source(&["C1"]);
    src.poll().await.unwrap();

    let root = s.ts(1);
    let noise = [
        "channel_join",
        "channel_leave",
        "bot_add",
        "channel_topic",
        "pinned_item",
    ];
    for (i, sub) in noise.iter().enumerate() {
        s.post_raw(
            "C1",
            json!({"type": "message", "subtype": sub, "ts": s.ts(10 + i as u64), "user": "U2", "text": "<@U2> has joined"}),
        );
    }
    s.post_raw("C1", json!({
        "type": "message", "subtype": "file_share", "ts": s.ts(20), "user": "U1", "text": "logs attached",
        "files": [{"name": "app.log"}, {"title": "screenshot"}]
    }));
    s.post_raw(
        "C1",
        json!({
            "type": "message", "ts": s.ts(21), "user": "U2", "text": "fixed typo",
            "edited": {"user": "U2", "ts": s.ts(22)}
        }),
    );
    s.post_raw(
        "C1",
        json!({
            "type": "message", "subtype": "thread_broadcast", "ts": s.ts(23), "thread_ts": root,
            "user": "U1", "text": "also in channel"
        }),
    );
    s.post_raw(
        "C1",
        json!({
            "type": "message", "subtype": "message_changed", "ts": s.ts(24),
            "message": {"type": "message", "ts": s.ts(1), "user": "U1", "text": "seed (edited)"}
        }),
    );
    s.post_raw(
        "C1",
        json!({
            "type": "message", "subtype": "bot_message", "ts": s.ts(25), "bot_id": "B1",
            "username": "CI", "text": "build green"
        }),
    );
    s.post_raw(
        "C1",
        json!({"type": "message", "ts": s.ts(26), "user": "U1", "text": ""}),
    );

    let msgs = src.poll().await.unwrap();
    assert_eq!(
        texts(&msgs),
        [
            "seed (edited)",
            "logs attached\n[files: app.log, screenshot]",
            "fixed typo",
            "also in channel",
            "build green",
            ""
        ]
    );
    let by_text = |t: &str| msgs.iter().find(|m| m.text == t).unwrap();
    assert!(by_text("fixed typo").edited);
    assert!(by_text("seed (edited)").edited);
    assert_eq!(by_text("seed (edited)").id, s.ts(1));
    assert!(!by_text("build green").edited);
    assert_eq!(
        by_text("also in channel").thread_id.as_deref(),
        Some(root.as_str())
    );
    assert_eq!(by_text("build green").author_id.as_deref(), Some("B1"));
    assert_eq!(by_text("build green").author_name.as_deref(), Some("CI"));
    // The skipped noise still moved the cursor: nothing comes back.
    assert!(src.poll().await.unwrap().is_empty());
}

#[tokio::test]
async fn post_reply_sends_channel_thread_and_text_as_the_user() {
    let s = Slack::start().await;
    s.post("C1", 1, "U1", "hi");
    let src = s.source(&["C1"]);

    let ts = src
        .post_reply("C1", Some(&s.ts(1)), "on it <b>&")
        .await
        .unwrap();
    assert_eq!(ts, "1800000000.000001");
    let ts = src.post_reply("C1", None, "top level").await.unwrap();
    assert_eq!(ts, "1800000000.000001");
    {
        let m = s.mock.lock().unwrap();
        assert_eq!(
            m.posts[0],
            json!({"channel": "C1", "thread_ts": s.ts(1), "text": "on it <b>&"})
        );
        assert_eq!(m.posts[1], json!({"channel": "C1", "text": "top level"}));
    }
    s.assert_token_handled();
    // The posted reply's link, from Slack itself.
    let replier = src.replier().unwrap();
    assert_eq!(
        replier
            .permalink("C1", "1800000000.000001")
            .await
            .as_deref(),
        Some("https://acme.slack.com/archives/C1/p1800000000000001")
    );

    assert_eq!(
        src.post_reply("C404", None, "x").await,
        Err(FeedError::Api("not_in_channel".into()))
    );
    s.fault(
        "chat.postMessage",
        Box::new(|_| true),
        FaultKind::TooMany(7),
    );
    assert_eq!(
        src.post_reply("C1", None, "x").await,
        Err(FeedError::RateLimited(Duration::from_secs(7)))
    );
}

#[tokio::test]
async fn api_errors_are_reported() {
    let s = Slack::start().await;
    s.post("C1", 1, "U1", "hi");

    let mut bad = SlackSource::new("xoxp-wrong", vec!["C1".into()]).with_base_url(&s.url);
    assert_eq!(bad.poll().await, Err(FeedError::Api("invalid_auth".into())));

    // One unreachable channel does not hide the others.
    let mut src = s.source(&["C404", "C1"]);
    assert_eq!(texts(&src.poll().await.unwrap()), ["hi"]);
    assert_eq!(
        src.last_errors(),
        [(
            "C404".to_owned(),
            FeedError::Api("channel_not_found".into())
        )]
    );

    // But when every channel fails, the poll fails.
    let mut lost = s.source(&["C404"]);
    assert_eq!(
        lost.poll().await,
        Err(FeedError::Api("channel_not_found".into()))
    );
}

#[tokio::test]
async fn suggested_interval_backs_off_when_idle() {
    let s = Slack::start().await;
    s.post("C1", 1, "U1", "hi");
    let mut src = s
        .source(&["C1"])
        .with_poll_interval(Duration::from_secs(15), Duration::from_secs(120));
    assert_eq!(src.suggested_interval(), Duration::from_secs(15));
    src.poll().await.unwrap();
    assert_eq!(src.suggested_interval(), Duration::from_secs(15));
    let mut seen = Vec::new();
    for _ in 0..4 {
        src.poll().await.unwrap();
        seen.push(src.suggested_interval().as_secs());
    }
    assert_eq!(seen, [30, 60, 120, 120]);
    s.post("C1", 2, "U1", "again");
    src.poll().await.unwrap();
    assert_eq!(src.suggested_interval(), Duration::from_secs(15));
}

#[tokio::test]
async fn polled_messages_carry_kind_and_owner_flags() {
    let s = Slack::start().await;
    s.post("C1", 1, "U1", "ping <@UME>");
    s.post("C1", 2, "UME", "pong <@UME>");
    s.post("C1", 3, "U2", "<@UMEX> is someone else");
    let mut src = s.source(&["C1"]);
    let msgs = src.poll().await.unwrap();
    assert!(
        msgs.iter()
            .all(|m| m.conversation_kind == ConversationKind::Channel)
    );
    let flags: Vec<(bool, bool)> = msgs
        .iter()
        .map(|m| (m.mentions_owner, m.from_owner))
        .collect();
    assert_eq!(flags, [(true, false), (false, true), (false, false)]);
    let caps = src.capabilities();
    assert!(caps.poll && caps.can_reply && !caps.push);
    assert!(src.replier().is_some());
    // The polling run stops only when the token is unusable; everything else is retried.
    assert!(src.is_fatal(&FeedError::Api("token_revoked".into())));
    assert!(!src.is_fatal(&FeedError::Api("internal_error".into())));
    assert!(!src.is_fatal(&FeedError::Api("channel_not_found".into())));
    assert!(!src.is_fatal(&FeedError::Http("timeout".into())));
}

#[tokio::test]
async fn a_mapped_channel_the_owner_is_not_in_is_not_polled() {
    let s = Slack::start().await;
    s.post("C1", 1, "U1", "member");
    s.post("C2", 2, "U1", "outsider");
    s.mock.lock().unwrap().non_members.insert("C2".into());
    let mut src = s.source(&["C1", "C2"]);
    assert_eq!(texts(&src.poll().await.unwrap()), ["member"]);
    assert_eq!(
        src.last_errors(),
        [("C2".to_owned(), FeedError::Api("not_in_channel".into()))]
    );
    assert!(
        s.requests("conversations.history").iter().all(|r| r
            .query
            .get("channel")
            .map(String::as_str)
            != Some("C2")),
        "its history is never read"
    );
}

#[tokio::test]
async fn the_default_run_polls_into_the_sink_with_checkpoints() {
    let s = Slack::start().await;
    s.post("C1", 1, "U1", "one");
    let mut src = s
        .source(&["C1"])
        .with_poll_interval(Duration::from_millis(20), Duration::from_millis(20));
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let run = tokio::spawn(async move { src.run(tx).await });
    async fn next(rx: &mut tokio::sync::mpsc::Receiver<FeedEvent>) -> FeedEvent {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap()
    }
    match next(&mut rx).await {
        FeedEvent::Message(m) => assert_eq!(m.text, "one"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(next(&mut rx).await, FeedEvent::Checkpoint(_)));
    s.post("C1", 2, "U1", "two");
    loop {
        match next(&mut rx).await {
            FeedEvent::Message(m) => {
                assert_eq!(m.text, "two");
                break;
            }
            FeedEvent::Checkpoint(_) => continue,
            other => panic!("{other:?}"),
        }
    }
    // A recoverable error is retried, not the end of the run.
    s.fault(
        "conversations.history",
        Box::new(|_| true),
        FaultKind::ApiError("internal_error"),
    );
    s.post("C1", 3, "U1", "three");
    loop {
        match next(&mut rx).await {
            FeedEvent::Message(m) => {
                assert_eq!(m.text, "three");
                break;
            }
            FeedEvent::Checkpoint(_) => continue,
            other => panic!("{other:?}"),
        }
    }
    assert!(!run.is_finished());
    // Closing the sink stops the run cleanly.
    drop(rx);
    let res = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res, Ok(()));
}

#[tokio::test]
async fn rate_limit_in_a_thread_returns_the_replies_before_it() {
    let s = Slack::start().await;
    let root = s.post("C1", 1, "U1", "root");
    let mut src = s
        .source(&["C1"])
        .with_page_size(2)
        .with_thread_sweep_interval(Duration::ZERO);
    src.poll().await.unwrap();

    for i in 2..=6 {
        s.reply(&root, i, "U2", &format!("r{i}"));
    }
    // Replies come oldest first, so the pages before the limit are a run with no gap.
    s.fault(
        "conversations.replies",
        Box::new(|q| q.contains_key("cursor")),
        FaultKind::TooMany(1),
    );
    let msgs = src.poll().await.unwrap();
    assert_eq!(
        texts(&msgs),
        ["r2"],
        "the root and the first reply fill the first page"
    );
    assert!(src.rate_limited_for().is_some());

    tokio::time::sleep(Duration::from_millis(1100)).await;
    let msgs = src.poll().await.unwrap();
    assert_eq!(texts(&msgs), ["r3", "r4", "r5", "r6"]);
    assert!(src.poll().await.unwrap().is_empty());
}
