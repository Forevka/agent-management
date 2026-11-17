//! End-to-end: a real orchestrator (Postgres, fake chat) and a real node over iroh on loopback,
//! running a scripted fake `claude` through the whole task lifecycle.
//!
//! Needs `TEST_DATABASE_URL` (a Postgres the test may create databases in) and `python3`;
//! skipped otherwise, so plain `cargo test` works anywhere.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use fleet_net::{
    Client, ClientConfig, EndpointAddr, RelayChoice, SecretKey, Server, bind_endpoint,
};
use fleet_proto::ALPN;
use iroh::protocol::Router;
use serde_json::{Value, json};

use crate::{
    Config, MIGRATOR, Orchestrator, Vault,
    auth::DbAuth,
    chat::fake::{FakeChat, Op},
    human::{self, Answer},
    model::{HumanItemRow, NewAgent, TaskRow},
    tasks::{self, CreateTask},
};

async fn test_db() -> Option<sqlx::PgPool> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let admin = sqlx::PgPool::connect(&url)
        .await
        .expect("TEST_DATABASE_URL unreachable");
    let name = format!(
        "fleet_e2e_{}",
        crate::web::random_token()
            .to_lowercase()
            .replace(['-', '_'], "")
    );
    let name = &name[..name.len().min(40)];
    sqlx::query(&format!("create database {name}"))
        .execute(&admin)
        .await
        .unwrap();
    let base = url.rsplit_once('/').map(|(b, _)| b.to_owned()).unwrap();
    let db = sqlx::PgPool::connect(&format!("{base}/{name}"))
        .await
        .unwrap();
    MIGRATOR.run(&db).await.unwrap();
    Some(db)
}

fn has_python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

async fn wait_for<T, F, Fut>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn open_item(db: &sqlx::PgPool, task: &str, kind: &str) -> Option<HumanItemRow> {
    sqlx::query_as("select * from human_items where task_id = $1 and kind = $2 and state = 'open'")
        .bind(task)
        .bind(kind)
        .fetch_optional(db)
        .await
        .unwrap()
}

async fn events(db: &sqlx::PgPool, task: &str) -> Vec<Value> {
    let rows: Vec<String> = sqlx::query_scalar(
        "select data from task_events where task_id = $1 and kind = 'out' order by id",
    )
    .bind(task)
    .fetch_all(db)
    .await
    .unwrap();
    rows.iter()
        .filter_map(|r| serde_json::from_str(r).ok())
        .collect()
}

#[tokio::test]
async fn task_lifecycle_through_a_real_node() {
    let Some(db) = test_db().await else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };
    if !has_python() {
        eprintln!("skipped: python3 not available");
        return;
    }

    // Orchestrator.
    let vault = Vault::from_hex(&"11".repeat(32)).unwrap();
    let auth = DbAuth::new(db.clone());
    let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let ep = bind_endpoint(
        SecretKey::generate(),
        RelayChoice::Disabled,
        Some(loopback),
        vec![ALPN.to_vec()],
    )
    .await
    .unwrap();
    let port = ep
        .bound_sockets()
        .into_iter()
        .find(|s| s.is_ipv4())
        .unwrap()
        .port();
    let orch_addr =
        EndpointAddr::new(ep.id()).with_ip_addr(SocketAddr::from(([127, 0, 0, 1], port)));
    let (server, node_events) = Server::new(auth.clone());
    let _router = Router::builder(ep).accept(ALPN, server.clone()).spawn();
    let chat = Arc::new(FakeChat::default());
    let o = Orchestrator::new(
        db.clone(),
        vault.clone(),
        server,
        chat.clone(),
        Config::test(),
        auth.clone(),
    );
    tokio::spawn(o.clone().run_node_events(node_events));

    // Project with a bound secret folder, and one pool bot.
    sqlx::query("insert into projects (slug, name) values ('demo', 'Demo')")
        .execute(&db)
        .await
        .unwrap();
    let folder: i64 = sqlx::query_scalar(
        "insert into secret_folders (name, preset) values ('Jira', 'jira') returning id",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    sqlx::query(
        "insert into secret_items (folder_id, key, value) values ($1, 'JIRA_API_TOKEN', $2)",
    )
    .bind(folder)
    .bind(vault.encrypt("t0k"))
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("insert into project_secrets (project, folder_id) values ('demo', $1)")
        .bind(folder)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("insert into bots (token, tg_user_id, username) values ($1, 777, 'alice_bot')")
        .bind(vault.encrypt("777:abc"))
        .execute(&db)
        .await
        .unwrap();

    // Node: joins with an invite, maps the project, runs the fake claude.
    let data_dir = std::env::temp_dir().join(format!("fleet-e2e-{}", crate::web::random_token()));
    let project_dir = data_dir.join("demo-repo");
    std::fs::create_dir_all(&project_dir).unwrap();
    let mut node_cfg = fleet_node::config::NodeConfig::default();
    node_cfg.map_project("demo", &project_dir).unwrap();
    node_cfg.save(&data_dir).unwrap();
    sqlx::query("insert into invites (secret) values ('inv')")
        .execute(&db)
        .await
        .unwrap();
    let node_ep = bind_endpoint(
        SecretKey::generate(),
        RelayChoice::Disabled,
        Some(loopback),
        vec![],
    )
    .await
    .unwrap();
    let node_id = node_ep.id().to_string();
    let (client, link_events) = Client::new(orch_addr.clone());
    let node = fleet_node::Node::new(
        data_dir.clone(),
        PathBuf::from("/nonexistent/fleet"),
        client.clone(),
    )
    .unwrap();
    tokio::spawn(node.run(link_events));
    let cfg = ClientConfig {
        orchestrator: orch_addr,
        node_name: "e2e-node".into(),
        app_version: "0".into(),
        os: "linux".into(),
        join_secret: Some("inv".into()),
    };
    tokio::spawn(async move { client.run(&node_ep, cfg).await });

    // Point the node at the fake claude once it has joined and reported its projects.
    wait_for("node info", || async {
        sqlx::query_scalar::<_, bool>(
            "select jsonb_array_length(projects) > 0 from nodes where id = $1",
        )
        .bind(&node_id)
        .fetch_optional(&db)
        .await
        .unwrap()
        .filter(|b| *b)
    })
    .await;
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake_claude.py");
    sqlx::query("update nodes set claude_command = $2 where id = $1")
        .bind(&node_id)
        .bind(sqlx::types::Json(vec![
            "python3".to_owned(),
            script.display().to_string(),
        ]))
        .execute(&db)
        .await
        .unwrap();
    o.push_node_settings(&node_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The brain would call this through create_task.
    let task: TaskRow = tasks::create_task(
        &o,
        CreateTask {
            project: "demo".into(),
            title: "Paint the bikeshed".into(),
            brief: "Pick a color and paint it.".into(),
            ticket: Some("BS-1".into()),
            agent: None,
            new_agent: Some(NewAgent {
                name: "Alice".into(),
                role: "implementor".into(),
            }),
        },
    )
    .await
    .unwrap();
    assert_eq!(task.id, "demo-BS-1");

    // 1. AskUserQuestion during planning, answered from the web page.
    let q = wait_for("question item", || open_item(&db, &task.id, "question")).await;
    let mut answers = serde_json::Map::new();
    answers.insert("Which color?".into(), json!("Blue"));
    human::answer(&o, q.id, Answer::Questions(answers))
        .await
        .unwrap();

    // 2. Plan approval.
    let plan = wait_for("plan item", || open_item(&db, &task.id, "plan")).await;
    assert!(
        plan.payload.0["plan"]
            .as_str()
            .unwrap()
            .contains("Paint it")
    );
    human::answer(&o, plan.id, Answer::Approve).await.unwrap();

    // 3. Milestones and a non-blocking-for-the-process question, answered by button.
    let ask = wait_for("ask item", || open_item(&db, &task.id, "ask")).await;
    human::answer(&o, ask.id, Answer::Choice(0)).await.unwrap();

    // 4. The agent completes.
    wait_for("task done", || async {
        o.task(&task.id)
            .await
            .unwrap()
            .filter(|t| t.status == "done")
    })
    .await;

    let ev = events(&db, &task.id).await;
    let find = |t: &str| {
        ev.iter()
            .find(|e| e["type"] == t)
            .cloned()
            .unwrap_or_else(|| panic!("no {t} event in {ev:#?}"))
    };
    assert_eq!(
        find("system")["permission_mode"],
        "plan",
        "new tasks start in plan mode"
    );
    assert_eq!(find("answers")["answers"]["Which color?"], "Blue");
    assert_eq!(find("plan_decision")["decision"]["behavior"], "allow");
    assert_eq!(
        find("control_seen")["request"]["mode"],
        "auto",
        "approval switches the session to auto"
    );
    assert_eq!(
        find("secrets")["files"],
        json!(["jira.env"]),
        "secrets land outside the project"
    );
    assert!(
        std::fs::read_dir(&project_dir).unwrap().next().is_none(),
        "nothing written into the project"
    );

    let task = o.task(&task.id).await.unwrap().unwrap();
    assert!(task.session_id.as_deref().unwrap().starts_with("sess-"));
    let ops = chat.ops.lock().unwrap().clone();
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::CreateTopic(n, _) if n.starts_with("demo-BS-1"))),
        "task topic"
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::CreateTopic(n, _) if n == "demo · needs you")),
        "human topic"
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::BotProfile(_, n) if n == "Alice · implementor")),
        "bot renamed"
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::Edit { html, .. } if html.contains("2/2"))),
        "checklist ticked"
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::Close(t) if Some(*t) == task.topic_id)),
        "topic closed"
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::Send { html, .. } if html.contains("Painted and shipped"))),
        "summary"
    );

    // 5. A follow-up resumes the same session on the same node.
    let follow_session = task.session_id.clone();
    wait_for("agent idle", || async {
        o.task(&task.id).await.unwrap().filter(|t| t.running)
    })
    .await;
    tasks::deliver(&o, &task, "[Message from the human] one more coat".into())
        .await
        .unwrap();
    wait_for("follow-up seen", || async {
        events(&db, &task.id)
            .await
            .iter()
            .find(|e| {
                e["type"] == "assistant"
                    && e["message"]["content"][0]["text"]
                        .as_str()
                        .unwrap_or("")
                        .contains("one more coat")
            })
            .cloned()
    })
    .await;
    assert_eq!(
        o.task(&task.id).await.unwrap().unwrap().session_id,
        follow_session
    );

    // 6. Stop kills the process and the node reports it.
    tasks::stop(&o, &task).await.unwrap();
    wait_for("agent stopped", || async {
        o.task(&task.id).await.unwrap().filter(|t| !t.running)
    })
    .await;
    assert!(
        !data_dir.join("secrets").join("demo-BS-1").exists(),
        "secrets removed when the agent exits"
    );
    let _ = std::fs::remove_dir_all(&data_dir);
}
