//! `GChatSource` against a mock Google (token endpoint, token-info and the Chat API) built with
//! wiremock. The mock answers as the Chat REST reference documents: `spaces.list` with
//! `lastActiveTime`, `spaces.messages.list` filtered by `createTime > "…"`, oldest first.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fleet_feeds::{ConversationKind, FeedError, GChatSource, GoogleCredentials, Source};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// An RFC 3339 timestamp `offset` seconds from now, with a fixed fraction.
fn at(offset: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + offset;
    // Days to a civil date, as the source does it, kept local to the test.
    let days = now.div_euclid(86_400);
    let rem = now.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.250000Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

struct Google {
    server: MockServer,
    spaces: Arc<Mutex<Value>>,
    messages: Arc<Mutex<Value>>,
}

impl Google {
    async fn start(expires_in: u64) -> Self {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=r3fresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"access_token": "at-1", "expires_in": expires_in, "token_type": "Bearer"}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/tokeninfo"))
            .and(body_string_contains("access_token=at-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sub": "111"})))
            .mount(&server)
            .await;
        let spaces = Arc::new(Mutex::new(json!({"spaces": []})));
        let s = spaces.clone();
        Mock::given(method("GET"))
            .and(path("/v1/spaces"))
            .and(header("authorization", "Bearer at-1"))
            .respond_with(move |_: &Request| {
                ResponseTemplate::new(200).set_body_json(s.lock().unwrap().clone())
            })
            .mount(&server)
            .await;
        let messages = Arc::new(Mutex::new(json!({})));
        let m = messages.clone();
        Mock::given(method("GET"))
            .and(path("/v1/spaces/AAA/messages"))
            .and(header("authorization", "Bearer at-1"))
            .respond_with(move |req: &Request| {
                // Applies the `createTime > "…"` filter the way the API does.
                let filter = req
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "filter")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                let since = filter.split('"').nth(1).unwrap_or("").to_owned();
                let all = m.lock().unwrap().clone();
                let kept: Vec<Value> = all["messages"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|msg| msg["createTime"].as_str().unwrap_or("") > since.as_str())
                    .collect();
                if kept.is_empty() {
                    ResponseTemplate::new(200).set_body_json(json!({}))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({ "messages": kept }))
                }
            })
            .mount(&server)
            .await;
        Self {
            server,
            spaces,
            messages,
        }
    }

    fn creds(&self) -> GoogleCredentials {
        GoogleCredentials {
            client_id: "cid".into(),
            client_secret: "s3cret".into(),
            refresh_token: "r3fresh".into(),
            token_uri: format!("{}/token", self.server.uri()),
        }
    }

    fn source(&self) -> GChatSource {
        GChatSource::new(self.creds())
            .with_endpoints(
                &self.server.uri(),
                &format!("{}/tokeninfo", self.server.uri()),
            )
            .with_request_gap(Duration::ZERO)
    }

    async fn requests(&self, p: &str) -> Vec<Request> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.url.path() == p)
            .collect()
    }
}

fn space(name: &str, kind: &str, display: &str, last_active: &str) -> Value {
    json!({
        "name": name, "spaceType": kind, "displayName": display,
        "lastActiveTime": last_active,
        "spaceUri": format!("https://chat.google.com/room/{}", name.trim_start_matches("spaces/")),
        "spaceThreadingState": if kind == "SPACE" { "THREADED_MESSAGES" } else { "UNTHREADED_MESSAGES" }
    })
}

#[tokio::test]
async fn polls_only_active_spaces_from_where_it_left_off() {
    let g = Google::start(3599).await;
    *g.spaces.lock().unwrap() = json!({"spaces": [
        space("spaces/AAA", "SPACE", "eng", &at(-3600)),
        space("spaces/BBB", "SPACE", "quiet", &at(-3600)),
    ]});
    let mut src = g.source();

    // The first poll only marks where watching begins: no history.
    assert!(src.poll().await.unwrap().is_empty());
    assert!(g.requests("/v1/spaces/AAA/messages").await.is_empty());
    assert_eq!(src.owner().as_deref(), Some("users/111"));

    // Activity in AAA only.
    let (t1, t2, t3) = (at(5), at(6), at(7));
    *g.spaces.lock().unwrap() = json!({"spaces": [
        space("spaces/AAA", "SPACE", "eng", &t3),
        space("spaces/BBB", "SPACE", "quiet", &at(-3600)),
    ]});
    *g.messages.lock().unwrap() = json!({"messages": [
        {"name": "spaces/AAA/messages/m1", "createTime": t1, "text": "deploy at 5?",
         "sender": {"name": "users/222", "displayName": "Bob", "type": "HUMAN"},
         "thread": {"name": "spaces/AAA/threads/t1"}},
        {"name": "spaces/AAA/messages/m2", "createTime": t2, "text": "@Owner can you check",
         "sender": {"name": "users/333", "displayName": "Carol", "type": "HUMAN"},
         "annotations": [{"type": "USER_MENTION", "startIndex": 0, "length": 6,
                          "userMention": {"user": {"name": "users/111", "type": "HUMAN"}, "type": "MENTION"}}],
         "thread": {"name": "spaces/AAA/threads/t2"}},
        {"name": "spaces/AAA/messages/m3", "createTime": t3, "lastUpdateTime": at(9),
         "text": "on it", "sender": {"name": "users/111", "displayName": "Owner", "type": "HUMAN"},
         "thread": {"name": "spaces/AAA/threads/t1"}}
    ]});
    let msgs = src.poll().await.unwrap();
    let texts: Vec<&str> = msgs.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["deploy at 5?", "@Owner can you check", "on it"]);
    let first = &msgs[0];
    assert_eq!(first.source, "gchat");
    assert_eq!(first.conversation_id, "spaces/AAA");
    assert_eq!(first.conversation_name.as_deref(), Some("eng"));
    assert_eq!(first.conversation_kind, ConversationKind::Channel);
    assert_eq!(first.id, "spaces/AAA/messages/m1");
    assert_eq!(first.thread_id.as_deref(), Some("spaces/AAA/threads/t1"));
    assert_eq!(first.author_id.as_deref(), Some("users/222"));
    assert_eq!(first.author_name.as_deref(), Some("Bob"));
    assert_eq!(
        first.permalink.as_deref(),
        Some("https://chat.google.com/room/AAA")
    );
    assert!(!first.mentions_owner && !first.from_owner && !first.edited);
    assert!(msgs[1].mentions_owner);
    assert!(msgs[2].from_owner && !msgs[2].mentions_owner);
    assert!(msgs[2].edited);
    assert_eq!(msgs[0].conversation_attention, None);
    // Only the active space was read, and with the filter from the start of watching.
    assert!(g.requests("/v1/spaces/BBB/messages").await.is_empty());
    let reads = g.requests("/v1/spaces/AAA/messages").await;
    assert_eq!(reads.len(), 1);
    let filter: String = reads[0]
        .url
        .query_pairs()
        .find(|(k, _)| k == "filter")
        .unwrap()
        .1
        .into_owned();
    assert!(filter.starts_with("createTime > \""), "{filter}");

    // The cursor moved to the newest message: a quiet poll reads no messages, and the cursor
    // survives a restart.
    assert!(src.poll().await.unwrap().is_empty());
    assert_eq!(g.requests("/v1/spaces/AAA/messages").await.len(), 1);
    let saved = src.cursor();
    assert_eq!(saved["spaces"]["spaces/AAA"], json!(t3));
    let mut again = g.source();
    again.restore_cursor(saved);
    assert!(again.poll().await.unwrap().is_empty());

    // One token served every request.
    assert_eq!(
        g.requests("/token").await.len(),
        2,
        "one per source instance"
    );
}

#[tokio::test]
async fn an_expiring_token_is_refreshed_before_use() {
    // Tokens that expire within the safety margin are renewed for every request.
    let g = Google::start(30).await;
    let mut src = g.source();
    src.poll().await.unwrap();
    src.poll().await.unwrap();
    assert!(g.requests("/token").await.len() >= 3);
    // Long-lived tokens are reused.
    let g = Google::start(3599).await;
    let mut src = g.source();
    src.poll().await.unwrap();
    src.poll().await.unwrap();
    assert_eq!(g.requests("/token").await.len(), 1);
}

#[tokio::test]
async fn a_revoked_refresh_token_is_fatal() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            json!({"error": "invalid_grant", "error_description": "Token has been expired or revoked."}),
        ))
        .mount(&server)
        .await;
    let creds = GoogleCredentials {
        client_id: "cid".into(),
        client_secret: "s3cret".into(),
        refresh_token: "r3fresh".into(),
        token_uri: format!("{}/token", server.uri()),
    };
    let mut src = GChatSource::new(creds)
        .with_endpoints(&server.uri(), &format!("{}/tokeninfo", server.uri()))
        .with_request_gap(Duration::ZERO);
    let err = src.poll().await.unwrap_err();
    assert_eq!(err, FeedError::Api("invalid_grant".into()));
    assert!(src.is_fatal(&err));
    assert!(!src.is_fatal(&FeedError::Http("timeout".into())));
    assert!(!src.is_fatal(&FeedError::Api("PERMISSION_DENIED".into())));
    // And the default run stops with it.
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    assert_eq!(
        src.run(tx).await,
        Err(FeedError::Api("invalid_grant".into()))
    );
}

#[tokio::test]
async fn replies_go_into_the_thread() {
    let g = Google::start(3599).await;
    *g.spaces.lock().unwrap() = json!({"spaces": [
        space("spaces/AAA", "SPACE", "eng", &at(-3600)),
        space("spaces/DM1", "DIRECT_MESSAGE", "", &at(-3600)),
    ]});
    Mock::given(method("POST"))
        .and(path("/v1/spaces/AAA/messages"))
        .and(header("authorization", "Bearer at-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"name": "spaces/AAA/messages/r1", "thread": {"name": "spaces/AAA/threads/t1"}}),
        ))
        .mount(&g.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/spaces/DM1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"name": "spaces/DM1/messages/r2"})),
        )
        .mount(&g.server)
        .await;
    let mut src = g.source();
    src.poll().await.unwrap();
    let replier = src.replier().unwrap();
    let id = replier
        .post_reply("spaces/AAA", Some("spaces/AAA/threads/t1"), "on it")
        .await
        .unwrap();
    assert_eq!(id, "spaces/AAA/messages/r1");
    let posts = g.requests("/v1/spaces/AAA/messages").await;
    let post = posts.iter().find(|r| r.method.as_str() == "POST").unwrap();
    let option: Vec<(String, String)> = post
        .url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(
        option,
        [(
            "messageReplyOption".to_owned(),
            "REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD".to_owned()
        )]
    );
    let body: Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(
        body,
        json!({"text": "on it", "thread": {"name": "spaces/AAA/threads/t1"}})
    );
    // A direct message has no threads to reply into: the text simply goes to it.
    replier
        .post_reply("spaces/DM1", Some("spaces/DM1/threads/x"), "sure")
        .await
        .unwrap();
    let dm = g.requests("/v1/spaces/DM1/messages").await;
    assert_eq!(dm[0].url.query(), None);
    let body: Value = serde_json::from_slice(&dm[0].body).unwrap();
    assert_eq!(body, json!({"text": "sure"}));
    let caps = src.capabilities();
    assert!(caps.poll && caps.can_reply && caps.can_reply_in_thread && !caps.push);

    // Given a message instead of a thread, the reply goes into that message's thread.
    Mock::given(method("GET"))
        .and(path("/v1/spaces/AAA/messages/m9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"name": "spaces/AAA/messages/m9", "thread": {"name": "spaces/AAA/threads/t9"}}),
        ))
        .mount(&g.server)
        .await;
    replier
        .post_reply("spaces/AAA", Some("spaces/AAA/messages/m9"), "following up")
        .await
        .unwrap();
    let posts = g.requests("/v1/spaces/AAA/messages").await;
    let last = posts.iter().rfind(|r| r.method.as_str() == "POST").unwrap();
    let body: Value = serde_json::from_slice(&last.body).unwrap();
    assert_eq!(body["thread"]["name"], "spaces/AAA/threads/t9");
    // The closest link Google documents is the space's.
    assert_eq!(
        replier
            .permalink("spaces/AAA", "spaces/AAA/messages/r1")
            .await
            .as_deref(),
        Some("https://chat.google.com/room/AAA")
    );
}

#[tokio::test]
async fn the_owner_is_recognised_from_direct_messages_without_token_info() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "at-1", "expires_in": 3599})),
        )
        .mount(&server)
        .await;
    // Token-info has no account id for this token.
    Mock::given(method("POST"))
        .and(path("/tokeninfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"scope": "x"})))
        .mount(&server)
        .await;
    let spaces = Arc::new(Mutex::new(json!({"spaces": []})));
    let s = spaces.clone();
    Mock::given(method("GET"))
        .and(path("/v1/spaces"))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(200).set_body_json(s.lock().unwrap().clone())
        })
        .mount(&server)
        .await;
    let (t1, t2, t3) = (at(5), at(6), at(7));
    for (dm, other, who, text, t) in [
        ("DM1", "users/222", "Bob", "hi from bob", &t1),
        ("DM2", "users/333", "Carol", "hi from carol", &t2),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/v1/spaces/{dm}/messages")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"messages": [
                    {"name": format!("spaces/{dm}/messages/a"), "createTime": t, "text": text,
                     "sender": {"name": other, "displayName": who, "type": "HUMAN"}},
                    {"name": format!("spaces/{dm}/messages/b"), "createTime": t3, "text": "reply",
                     "sender": {"name": "users/111", "displayName": "Owner", "type": "HUMAN"}}
                ]})),
            )
            .mount(&server)
            .await;
    }
    let creds = GoogleCredentials {
        client_id: "cid".into(),
        client_secret: "s3cret".into(),
        refresh_token: "r3fresh".into(),
        token_uri: format!("{}/token", server.uri()),
    };
    let mut src = GChatSource::new(creds)
        .with_endpoints(&server.uri(), &format!("{}/tokeninfo", server.uri()))
        .with_request_gap(Duration::ZERO);
    src.poll().await.unwrap();
    assert_eq!(src.owner(), None);
    *spaces.lock().unwrap() = json!({"spaces": [
        space("spaces/DM1", "DIRECT_MESSAGE", "", &t3),
        space("spaces/DM2", "DIRECT_MESSAGE", "", &t3),
    ]});
    let msgs = src.poll().await.unwrap();
    // users/111 wrote in two different direct messages: that can only be the owner.
    assert_eq!(src.owner().as_deref(), Some("users/111"));
    assert_eq!(msgs.len(), 4);
    let dir = src.directory().unwrap();
    let dm1 = dir.conversation("spaces/DM1").await.unwrap();
    assert_eq!(dm1.kind, ConversationKind::Direct);
    assert_eq!(
        dm1.name.as_deref(),
        Some("Bob"),
        "named after the other person"
    );
    // Direct messages are not threaded: no thread id.
    assert!(msgs.iter().all(|m| m.thread_id.is_none()));
    // The residual limitation this in-band path still has: these two direct-message spaces
    // appeared only after the first poll (an empty `spaces.list` at cold start, so there was
    // nothing for `bootstrap_owner_from_dms` to learn from), so the owner is recognised only
    // once DM2's own message is reached, mid-poll; DM1's owner message, processed earlier in
    // the same poll, is stamped `from_owner: false`. A cold start where the direct-message
    // spaces already exist does not have this gap (see
    // `the_owner_is_learned_from_direct_messages_before_any_message_is_delivered`).
    let by_id = |id: &str| msgs.iter().find(|m| m.id == id).unwrap();
    assert!(!by_id("spaces/DM1/messages/b").from_owner, "the known gap");
    assert!(
        by_id("spaces/DM2/messages/b").from_owner,
        "owner known by now"
    );
}

#[tokio::test]
async fn the_owner_is_learned_from_direct_messages_before_any_message_is_delivered() {
    // Cold start, as a real account has it: the direct-message spaces already exist (each one
    // needs at least one message to be listed at all), and token-info gives no account id. If
    // nothing resolved the owner before `cursor.start` is set, the first real messages in these
    // spaces would be mislabelled exactly as in
    // `the_owner_is_recognised_from_direct_messages_without_token_info` above.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "at-1", "expires_in": 3599})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tokeninfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"scope": "x"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/spaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"spaces": [
            space("spaces/DM1", "DIRECT_MESSAGE", "", &at(-100)),
            space("spaces/DM2", "DIRECT_MESSAGE", "", &at(-100)),
            space("spaces/DM3", "DIRECT_MESSAGE", "", &at(-100)),
            space("spaces/DM4", "DIRECT_MESSAGE", "", &at(-100)),
        ]})))
        .mount(&server)
        .await;
    // Each space's single most recent message, as `pageSize=1&orderBy=createTime desc` returns
    // it. users/111 (the owner) is the latest sender in DM2 and DM3: the second of those is
    // what resolves the owner, so DM4 must never be queried.
    for (dm, sender_id, name) in [
        ("DM1", "users/222", "Bob"),
        ("DM2", "users/111", "Owner"),
        ("DM3", "users/111", "Owner"),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/v1/spaces/{dm}/messages")))
            .and(query_param("pageSize", "1"))
            .and(query_param("orderBy", "createTime desc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"messages": [
                {"name": format!("spaces/{dm}/messages/latest"), "createTime": at(-50),
                 "text": "hi", "sender": {"name": sender_id, "displayName": name, "type": "HUMAN"}}
            ]})))
            .mount(&server)
            .await;
    }
    let creds = GoogleCredentials {
        client_id: "cid".into(),
        client_secret: "s3cret".into(),
        refresh_token: "r3fresh".into(),
        token_uri: format!("{}/token", server.uri()),
    };
    let mut src = GChatSource::new(creds)
        .with_endpoints(&server.uri(), &format!("{}/tokeninfo", server.uri()))
        .with_request_gap(Duration::ZERO);

    let msgs = src.poll().await.unwrap();
    assert!(msgs.is_empty(), "the first poll still imports no history");
    assert_eq!(
        src.owner().as_deref(),
        Some("users/111"),
        "learned before cursor.start is set, not after two in-band messages next poll"
    );
    let requests = server.received_requests().await.unwrap_or_default();
    let gets = |p: &str| {
        requests
            .iter()
            .filter(|r| r.method.as_str() == "GET" && r.url.path() == p)
            .count()
    };
    assert_eq!(gets("/v1/spaces/DM1/messages"), 1);
    assert_eq!(gets("/v1/spaces/DM2/messages"), 1);
    assert_eq!(gets("/v1/spaces/DM3/messages"), 1);
    assert_eq!(
        gets("/v1/spaces/DM4/messages"),
        0,
        "stops once the owner is known"
    );
}
