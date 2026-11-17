//! Exercises `Bot` and `Throttled` against a mock Bot API server on 127.0.0.1.

use std::time::{Duration, Instant};

use fleet_telegram::{
    Bot, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode, SendMessage, TgError, Throttled,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TOKEN: &str = "123456:TEST-token";
const GROUP: i64 = -1001234567890;

fn api_path(method_name: &str) -> String {
    format!("/bot{TOKEN}/{method_name}")
}

fn ok(result: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "result": result }))
}

fn api_error(code: u16, description: &str) -> ResponseTemplate {
    ResponseTemplate::new(code).set_body_json(json!({
        "ok": false,
        "error_code": code,
        "description": description,
    }))
}

fn sent_message(message_id: i64, thread_id: Option<i64>, text: &str) -> Value {
    let mut message = json!({
        "message_id": message_id,
        "date": 1_700_000_000,
        "chat": { "id": GROUP, "type": "supergroup", "title": "Fleet", "is_forum": true },
        "from": { "id": 123456, "is_bot": true, "first_name": "agent-1", "username": "agent1_bot" },
        "text": text,
    });
    if let Some(thread_id) = thread_id {
        message["message_thread_id"] = json!(thread_id);
        message["is_topic_message"] = json!(true);
    }
    message
}

async fn bot(server: &MockServer) -> Bot {
    Bot::with_base_url(TOKEN, server.uri()).expect("client builds")
}

async fn requests_to(server: &MockServer, method_name: &str) -> Vec<Request> {
    let wanted = api_path(method_name);
    server
        .received_requests()
        .await
        .expect("request recording is on")
        .into_iter()
        .filter(|request| request.url.path() == wanted)
        .collect()
}

fn json_body(request: &Request) -> Value {
    serde_json::from_slice(&request.body).expect("request body is JSON")
}

#[tokio::test]
async fn get_me_decodes_user() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(api_path("getMe")))
        .respond_with(ok(json!({
            "id": 123456, "is_bot": true, "first_name": "Orchestrator", "username": "fleet_bot",
            "can_join_groups": true, "supports_inline_queries": false
        })))
        .mount(&server)
        .await;
    let me = bot(&server).await.get_me().await.expect("getMe succeeds");
    assert_eq!(me.id, 123456);
    assert!(me.is_bot);
    assert_eq!(me.username.as_deref(), Some("fleet_bot"));
}

#[tokio::test]
async fn get_updates_decodes_topic_message_and_callback() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(api_path("getUpdates")))
        .respond_with(ok(json!([
            {
                "update_id": 1000,
                "message": {
                    "message_id": 55,
                    "message_thread_id": 42,
                    "is_topic_message": true,
                    "date": 1_700_000_100,
                    "chat": { "id": GROUP, "type": "supergroup", "title": "Fleet", "is_forum": true },
                    "from": { "id": 777, "is_bot": false, "first_name": "Ann", "language_code": "en" },
                    "text": "please run the tests",
                    "entities": [],
                    "reply_to_message": {
                        "message_id": 50,
                        "message_thread_id": 42,
                        "is_topic_message": true,
                        "date": 1_700_000_050,
                        "chat": { "id": GROUP, "type": "supergroup", "title": "Fleet", "is_forum": true },
                        "from": { "id": 123456, "is_bot": true, "first_name": "agent-1" },
                        "text": "Done: 3 files changed"
                    }
                }
            },
            {
                "update_id": 1001,
                "callback_query": {
                    "id": "cbq-9",
                    "chat_instance": "-555",
                    "from": { "id": 777, "is_bot": false, "first_name": "Ann" },
                    "data": "perm|a1|allow",
                    "message": sent_message(60, Some(42), "Allow Bash(rm -rf target)?")
                }
            },
            { "update_id": 1002, "my_chat_member": { "unmodelled": true } }
        ])))
        .mount(&server)
        .await;

    let updates = bot(&server)
        .await
        .get_updates(Some(1000), 30, &["message", "callback_query"])
        .await
        .expect("getUpdates succeeds");

    let request = &requests_to(&server, "getUpdates").await[0];
    assert_eq!(
        json_body(request),
        json!({ "offset": 1000, "timeout": 30, "allowed_updates": ["message", "callback_query"] })
    );

    assert_eq!(updates.len(), 3);
    let message = updates[0]
        .message
        .as_ref()
        .expect("first update is a message");
    assert_eq!(updates[0].update_id, 1000);
    assert_eq!(message.message_id, 55);
    assert_eq!(message.topic_id(), Some(42));
    assert_eq!(message.chat.kind, "supergroup");
    assert!(message.chat.is_forum);
    assert_eq!(message.from.as_ref().map(|u| u.id), Some(777));
    assert_eq!(message.text.as_deref(), Some("please run the tests"));
    let replied = message.reply_to_message.as_ref().expect("reply decoded");
    assert_eq!(replied.message_id, 50);
    assert_eq!(replied.text.as_deref(), Some("Done: 3 files changed"));

    let query = updates[1]
        .callback_query
        .as_ref()
        .expect("second update is a callback");
    assert_eq!(query.id, "cbq-9");
    assert_eq!(query.data.as_deref(), Some("perm|a1|allow"));
    assert_eq!(query.message.as_ref().map(|m| m.message_id), Some(60));

    // Kinds the crate does not model still decode, with nothing set.
    assert_eq!(updates[2].update_id, 1002);
    assert!(updates[2].message.is_none() && updates[2].callback_query.is_none());
}

#[tokio::test]
async fn send_message_serializes_thread_html_and_keyboard() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(api_path("sendMessage")))
        .respond_with(ok(sent_message(77, Some(42), "Allow?")))
        .mount(&server)
        .await;

    let keyboard = InlineKeyboardMarkup::single_row(vec![
        InlineKeyboardButton::callback("Allow", "perm|a1|allow"),
        InlineKeyboardButton::callback("Deny", "perm|a1|deny"),
    ])
    .row(vec![InlineKeyboardButton::url(
        "Diff",
        "https://example.com/diff",
    )]);
    let request = SendMessage::new(GROUP, "<b>Allow</b> Bash?")
        .thread(42)
        .html()
        .keyboard(keyboard)
        .reply_to(55)
        .silent();
    let message = bot(&server)
        .await
        .send_message(&request)
        .await
        .expect("sendMessage succeeds");
    assert_eq!(message.message_id, 77);
    assert_eq!(message.topic_id(), Some(42));

    let body = json_body(&requests_to(&server, "sendMessage").await[0]);
    assert_eq!(
        body,
        json!({
            "chat_id": GROUP,
            "message_thread_id": 42,
            "text": "<b>Allow</b> Bash?",
            "parse_mode": "HTML",
            "link_preview_options": { "is_disabled": true },
            "reply_parameters": { "message_id": 55, "allow_sending_without_reply": true },
            "disable_notification": true,
            "reply_markup": { "inline_keyboard": [
                [
                    { "text": "Allow", "callback_data": "perm|a1|allow" },
                    { "text": "Deny", "callback_data": "perm|a1|deny" }
                ],
                [ { "text": "Diff", "url": "https://example.com/diff" } ]
            ] }
        })
    );
}

#[tokio::test]
async fn plain_send_message_omits_optional_fields() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(api_path("sendMessage")))
        .respond_with(ok(sent_message(1, None, "hi")))
        .mount(&server)
        .await;
    bot(&server)
        .await
        .send_message(&SendMessage::new(GROUP, "hi"))
        .await
        .expect("sendMessage succeeds");
    let body = json_body(&requests_to(&server, "sendMessage").await[0]);
    assert_eq!(
        body,
        json!({ "chat_id": GROUP, "text": "hi", "link_preview_options": { "is_disabled": true } })
    );
}

#[tokio::test]
async fn raw_bot_surfaces_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(api_path("sendMessage")))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "ok": false,
            "error_code": 429,
            "description": "Too Many Requests: retry after 17",
            "parameters": { "retry_after": 17 }
        })))
        .mount(&server)
        .await;
    let err = bot(&server)
        .await
        .send_message(&SendMessage::new(GROUP, "x"))
        .await
        .expect_err("429");
    assert!(matches!(err, TgError::RetryAfter(17)), "{err:?}");
}

#[tokio::test]
async fn throttled_retries_after_429() {
    let server = MockServer::start().await;
    // Mounted first so it is matched first; it answers once and then steps aside.
    Mock::given(method("POST"))
        .and(path(api_path("sendMessage")))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "ok": false,
            "error_code": 429,
            "description": "Too Many Requests: retry after 1",
            "parameters": { "retry_after": 1 }
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(api_path("sendMessage")))
        .respond_with(ok(sent_message(9, Some(42), "hello")))
        .mount(&server)
        .await;

    let throttled = Throttled::new(bot(&server).await);
    let start = Instant::now();
    let message = throttled
        .send_message(&SendMessage::new(GROUP, "hello").thread(42))
        .await
        .expect("succeeds on the second attempt");
    assert_eq!(message.message_id, 9);
    assert!(
        start.elapsed() >= Duration::from_secs(1),
        "retried after {:?}",
        start.elapsed()
    );
    assert_eq!(requests_to(&server, "sendMessage").await.len(), 2);
}

#[tokio::test]
async fn throttled_gives_up_after_three_attempts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(api_path("editMessageText")))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "ok": false,
            "error_code": 429,
            "description": "Too Many Requests: retry after 0",
            "parameters": { "retry_after": 0 }
        })))
        .mount(&server)
        .await;
    let throttled = Throttled::new(bot(&server).await);
    let err = throttled
        .edit_message_text(GROUP, 5, "x", None, None)
        .await
        .expect_err("every attempt is rate limited");
    assert!(matches!(err, TgError::RetryAfter(0)), "{err:?}");
    assert_eq!(requests_to(&server, "editMessageText").await.len(), 3);
}

#[tokio::test]
async fn message_not_modified_is_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(api_path("editMessageText")))
        .respond_with(api_error(
            400,
            "Bad Request: message is not modified: specified new message content and reply markup \
             are exactly the same as a current content and reply markup of the message",
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(api_path("editMessageReplyMarkup")))
        .respond_with(api_error(400, "Bad Request: message is not modified"))
        .mount(&server)
        .await;
    let bot = bot(&server).await;
    let keyboard =
        InlineKeyboardMarkup::single_row(vec![InlineKeyboardButton::callback("Stop", "stop|a1|")]);
    bot.edit_message_text(
        GROUP,
        5,
        "<i>same</i>",
        Some(ParseMode::Html),
        Some(&keyboard),
    )
    .await
    .expect("not modified counts as success");
    bot.edit_message_reply_markup(GROUP, 5, None)
        .await
        .expect("not modified counts as success");

    let body = json_body(&requests_to(&server, "editMessageText").await[0]);
    assert_eq!(body["parse_mode"], "HTML");
    assert_eq!(body["message_id"], 5);
    assert_eq!(
        body["reply_markup"]["inline_keyboard"][0][0]["callback_data"],
        "stop|a1|"
    );
}

#[tokio::test]
async fn other_bad_requests_are_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(api_path("editMessageText")))
        .respond_with(api_error(400, "Bad Request: message to edit not found"))
        .mount(&server)
        .await;
    let err = bot(&server)
        .await
        .edit_message_text(GROUP, 5, "x", None, None)
        .await
        .expect_err("400");
    match err {
        TgError::Api { code, description } => {
            assert_eq!(code, 400);
            assert!(description.contains("not found"));
        }
        other => panic!("unexpected error {other:?}"),
    }
}

#[tokio::test]
async fn topic_not_modified_is_success() {
    let server = MockServer::start().await;
    for name in ["closeForumTopic", "reopenForumTopic", "editForumTopic"] {
        Mock::given(method("POST"))
            .and(path(api_path(name)))
            .respond_with(api_error(400, "Bad Request: TOPIC_NOT_MODIFIED"))
            .mount(&server)
            .await;
    }
    let bot = bot(&server).await;
    bot.close_forum_topic(GROUP, 42)
        .await
        .expect("already closed is fine");
    bot.reopen_forum_topic(GROUP, 42)
        .await
        .expect("already open is fine");
    bot.edit_forum_topic(GROUP, 42, "agent-1")
        .await
        .expect("same name is fine");
    let body = json_body(&requests_to(&server, "editForumTopic").await[0]);
    assert_eq!(
        body,
        json!({ "chat_id": GROUP, "message_thread_id": 42, "name": "agent-1" })
    );
}

#[tokio::test]
async fn create_forum_topic_returns_thread_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(api_path("createForumTopic")))
        .respond_with(ok(
            json!({ "message_thread_id": 314, "name": "agent-7 · repo", "icon_color": 7322096 }),
        ))
        .mount(&server)
        .await;
    let topic = bot(&server)
        .await
        .create_forum_topic(GROUP, "agent-7 · repo")
        .await
        .expect("created");
    assert_eq!(topic.message_thread_id, 314);
    assert_eq!(topic.name, "agent-7 · repo");
    let body = json_body(&requests_to(&server, "createForumTopic").await[0]);
    assert_eq!(body, json!({ "chat_id": GROUP, "name": "agent-7 · repo" }));
}

#[tokio::test]
async fn send_document_uploads_multipart() {
    let server = MockServer::start().await;
    let mut reply = sent_message(88, Some(42), "");
    reply["document"] =
        json!({ "file_id": "F1", "file_unique_id": "U1", "file_name": "transcript.txt" });
    reply["caption"] = json!("full transcript");
    Mock::given(method("POST"))
        .and(path(api_path("sendDocument")))
        .respond_with(ok(reply))
        .mount(&server)
        .await;

    let contents = b"line one\nline two\n".to_vec();
    let message = bot(&server)
        .await
        .send_document(
            GROUP,
            Some(42),
            "transcript.txt",
            contents.clone(),
            Some("full transcript"),
        )
        .await
        .expect("upload succeeds");
    assert_eq!(message.message_id, 88);
    assert_eq!(message.caption.as_deref(), Some("full transcript"));

    let request = &requests_to(&server, "sendDocument").await[0];
    let content_type = request
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .expect("content-type header");
    assert!(
        content_type.starts_with("multipart/form-data; boundary="),
        "{content_type}"
    );
    let body = String::from_utf8_lossy(&request.body);
    assert!(
        body.contains("name=\"chat_id\"\r\n\r\n-1001234567890\r\n"),
        "{body}"
    );
    assert!(
        body.contains("name=\"message_thread_id\"\r\n\r\n42\r\n"),
        "{body}"
    );
    assert!(
        body.contains("name=\"caption\"\r\n\r\nfull transcript\r\n"),
        "{body}"
    );
    assert!(
        body.contains("name=\"document\"; filename=\"transcript.txt\""),
        "{body}"
    );
    assert!(body.contains("line one\nline two\n"), "{body}");
}

#[tokio::test]
async fn simple_methods_send_expected_parameters() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok(json!(true)))
        .mount(&server)
        .await;
    let bot = bot(&server).await;
    bot.answer_callback_query("cbq-1", Some("Allowed"))
        .await
        .expect("answered");
    bot.set_my_name("agent-3").await.expect("named");
    bot.set_my_description("Works on repo X")
        .await
        .expect("described");
    bot.set_my_short_description("Agent 3")
        .await
        .expect("described");
    bot.delete_message(GROUP, 12).await.expect("deleted");

    let bodies: Vec<(String, Value)> = server
        .received_requests()
        .await
        .expect("recording is on")
        .iter()
        .map(|r| {
            (
                r.url
                    .path()
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
                json_body(r),
            )
        })
        .collect();
    assert_eq!(
        bodies,
        vec![
            (
                "answerCallbackQuery".into(),
                json!({ "callback_query_id": "cbq-1", "text": "Allowed" })
            ),
            ("setMyName".into(), json!({ "name": "agent-3" })),
            (
                "setMyDescription".into(),
                json!({ "description": "Works on repo X" })
            ),
            (
                "setMyShortDescription".into(),
                json!({ "short_description": "Agent 3" })
            ),
            (
                "deleteMessage".into(),
                json!({ "chat_id": GROUP, "message_id": 12 })
            ),
        ]
    );
}

#[tokio::test]
async fn non_json_error_reports_http_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502).set_body_string("<html>Bad Gateway</html>"))
        .mount(&server)
        .await;
    let err = bot(&server).await.get_me().await.expect_err("502");
    assert!(matches!(err, TgError::Api { code: 502, .. }), "{err:?}");
}

#[tokio::test]
async fn ok_response_with_wrong_shape_is_decode_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok(json!({ "unexpected": 1 })))
        .mount(&server)
        .await;
    let err = bot(&server).await.get_me().await.expect_err("bad shape");
    assert!(matches!(err, TgError::Decode(_)), "{err:?}");
}

#[tokio::test]
async fn transport_errors_do_not_leak_the_token() {
    // Bind and drop a listener so the port is very likely closed.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("bind")
        .port();
    let bot = Bot::with_base_url(TOKEN, format!("http://127.0.0.1:{port}")).expect("client builds");
    let err = bot.get_me().await.expect_err("nothing listens there");
    assert!(matches!(err, TgError::Http(_)), "{err:?}");
    assert!(!format!("{err} {err:?}").contains("TEST-token"), "{err:?}");
    assert!(!format!("{bot:?}").contains("TEST-token"));
    assert_eq!(bot.id(), Some(123456));
}
