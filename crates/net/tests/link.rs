//! End-to-end tests of the control link over real iroh endpoints on loopback.

use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use fleet_net::{
    Authorizer, BoxFuture, Client, ClientConfig, EndpointAddr, EndpointId, Inbound, RelayChoice,
    SecretKey, Server, bind_endpoint,
};
use fleet_proto::{ALPN, Body, Hello, NodeInfo};
use iroh::{Endpoint, protocol::Router};
use tokio::sync::mpsc::UnboundedReceiver;

/// Allowlist plus one-time invite secrets, like the real orchestrator's.
#[derive(Clone, Default)]
struct TestAuth {
    allowed: Arc<Mutex<HashSet<EndpointId>>>,
    invites: Arc<Mutex<HashSet<String>>>,
}

impl Authorizer for TestAuth {
    fn authorize<'a>(
        &'a self,
        node: EndpointId,
        hello: &'a Hello,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if self.allowed.lock().unwrap().contains(&node) {
                return Ok(());
            }
            match &hello.join_secret {
                Some(s) if self.invites.lock().unwrap().remove(s) => {
                    self.allowed.lock().unwrap().insert(node);
                    Ok(())
                }
                _ => Err("unknown node".into()),
            }
        })
    }
}

fn loopback() -> Option<SocketAddr> {
    Some("127.0.0.1:0".parse().unwrap())
}

async fn start_orchestrator(
    auth: TestAuth,
) -> (Router, Server, UnboundedReceiver<Inbound>, EndpointAddr) {
    let ep = bind_endpoint(
        SecretKey::generate(),
        RelayChoice::Disabled,
        loopback(),
        vec![ALPN.to_vec()],
    )
    .await
    .unwrap();
    let addr = direct_addr(&ep);
    let (server, events) = Server::new(auth);
    let router = Router::builder(ep).accept(ALPN, server.clone()).spawn();
    (router, server, events, addr)
}

fn direct_addr(ep: &Endpoint) -> EndpointAddr {
    let port = ep
        .bound_sockets()
        .into_iter()
        .find(|s| s.is_ipv4())
        .unwrap()
        .port();
    EndpointAddr::new(ep.id()).with_ip_addr(SocketAddr::from(([127, 0, 0, 1], port)))
}

async fn node_endpoint(key: &SecretKey) -> Endpoint {
    bind_endpoint(key.clone(), RelayChoice::Disabled, loopback(), vec![])
        .await
        .unwrap()
}

fn cfg(orch: &EndpointAddr, join_secret: Option<&str>) -> ClientConfig {
    ClientConfig {
        orchestrator: orch.clone(),
        node_name: "test-node".into(),
        app_version: "0.0.0".into(),
        os: std::env::consts::OS.into(),
        join_secret: join_secret.map(Into::into),
    }
}

async fn next<T>(rx: &mut UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .expect("timed out")
        .expect("channel closed")
}

/// Next event that is a message, skipping connection notices.
async fn next_message(rx: &mut UnboundedReceiver<Inbound>) -> Body {
    loop {
        if let Inbound::Message { body, .. } = next(rx).await {
            return body;
        }
    }
}

fn text(i: usize) -> Body {
    Body::SendToAgent {
        agent_id: "a".into(),
        task_id: "t".into(),
        text: format!("m{i}"),
    }
}

fn info(n: u32) -> Body {
    Body::NodeInfo(NodeInfo {
        projects: vec![],
        max_agents: n,
        running_agents: vec![],
        running_runs: vec![],
    })
}

#[tokio::test]
async fn invited_node_joins_and_messages_flow_both_ways() {
    let auth = TestAuth::default();
    auth.invites.lock().unwrap().insert("invite-1".into());
    let (_router, server, mut orch_rx, orch_addr) = start_orchestrator(auth.clone()).await;

    let key = SecretKey::generate();
    let ep = node_endpoint(&key).await;
    let (client, mut node_rx) = Client::new(orch_addr.clone());
    let c = client.clone();
    let c_cfg = cfg(&orch_addr, Some("invite-1"));
    let run = tokio::spawn(async move { c.run(&ep, c_cfg).await });

    match next(&mut orch_rx).await {
        Inbound::Connected {
            peer,
            hello: Some(h),
        } => {
            assert_eq!(peer, key.public());
            assert_eq!(h.node_name, "test-node");
        }
        other => panic!("expected Connected, got {other:?}"),
    }
    assert!(
        auth.allowed.lock().unwrap().contains(&key.public()),
        "invite should allowlist the key"
    );
    assert!(
        auth.invites.lock().unwrap().is_empty(),
        "invite is single-use"
    );

    client.send(info(4));
    assert_eq!(next_message(&mut orch_rx).await, info(4));

    for i in 0..50 {
        server.send(key.public(), text(i));
    }
    for i in 0..50 {
        assert_eq!(next_message(&mut node_rx).await, text(i));
    }
    run.abort();
}

#[tokio::test]
async fn unknown_node_is_rejected() {
    let (_router, _server, _orch_rx, orch_addr) = start_orchestrator(TestAuth::default()).await;
    let ep = node_endpoint(&SecretKey::generate()).await;
    let (client, _rx) = Client::new(orch_addr.clone());
    let err = tokio::time::timeout(
        Duration::from_secs(20),
        client.run(&ep, cfg(&orch_addr, Some("bogus"))),
    )
    .await
    .expect("rejection should end run()")
    .unwrap_err();
    assert!(format!("{err:#}").contains("unknown node"), "{err:#}");
}

/// A node from before the current protocol is refused, and the refusal itself names the release
/// to update to: nothing else can reach a node in that state, so this is its only way back. The
/// node here speaks the handshake by hand, as an old binary would, since `Client` always sends the
/// current version.
#[tokio::test]
async fn an_old_node_is_refused_with_the_release_to_update_to() {
    let (_router, server, _orch_rx, orch_addr) = start_orchestrator(TestAuth::default()).await;
    let update = fleet_proto::UpdateAvailable {
        version: "v9.9.9".into(),
        base_url: "https://github.com/o/r/releases/download/v9.9.9".into(),
    };
    server.set_update_hint(Some(update.clone()));

    let ep = node_endpoint(&SecretKey::generate()).await;
    let reply = tokio::time::timeout(Duration::from_secs(20), async {
        let conn = ep.connect(orch_addr.clone(), ALPN).await.unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let hello = Hello {
            protocol_version: fleet_proto::PROTOCOL_VERSION - 1,
            node_name: "old".into(),
            app_version: "v0.2.3".into(),
            os: "windows".into(),
            join_secret: None,
            last_received_seq: 0,
            incarnation: 1,
            last_received_from: 0,
        };
        let bytes = fleet_proto::Envelope::control(0, Body::Hello(hello))
            .encode()
            .unwrap();
        send.write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .unwrap();
        send.write_all(&bytes).await.unwrap();
        let mut len = [0u8; 4];
        recv.read_exact(&mut len).await.unwrap();
        let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
        recv.read_exact(&mut buf).await.unwrap();
        fleet_proto::Envelope::decode(&buf).unwrap().body
    })
    .await
    .expect("the orchestrator should answer an old node");

    let Body::Rejected(r) = reply else {
        panic!("expected a refusal, got {reply:?}");
    };
    assert!(r.reason.contains("protocol version"), "{}", r.reason);
    assert_eq!(fleet_proto::parse_update_hint(&r.reason), Some(update));
}

#[tokio::test]
async fn messages_survive_reconnect_exactly_once_in_order() {
    let auth = TestAuth::default();
    let key = SecretKey::generate();
    auth.allowed.lock().unwrap().insert(key.public());
    let (_router, server, mut orch_rx, orch_addr) = start_orchestrator(auth).await;

    let (client, mut node_rx) = Client::new(orch_addr.clone());

    // First connection: exchange a little, then kill the node's endpoint mid-session.
    let ep1 = node_endpoint(&key).await;
    let (c, cf) = (client.clone(), cfg(&orch_addr, None));
    let ep1c = ep1.clone();
    let run1 = tokio::spawn(async move { c.run(&ep1c, cf).await });
    assert!(matches!(
        next(&mut orch_rx).await,
        Inbound::Connected { .. }
    ));
    for i in 0..10 {
        server.send(key.public(), text(i));
    }
    for i in 0..10 {
        assert_eq!(next_message(&mut node_rx).await, text(i));
    }
    run1.abort();
    ep1.close().await;

    // While the node is down, both sides keep queueing.
    for i in 10..30 {
        server.send(key.public(), text(i));
    }
    client.send(info(1));
    client.send(info(2));

    // Second connection, new socket, same identity and session.
    let ep2 = node_endpoint(&key).await;
    let (c, cf) = (client.clone(), cfg(&orch_addr, None));
    let run2 = tokio::spawn(async move { c.run(&ep2, cf).await });

    for i in 10..30 {
        assert_eq!(
            next_message(&mut node_rx).await,
            text(i),
            "orchestrator -> node replay"
        );
    }
    assert_eq!(next_message(&mut orch_rx).await, info(1));
    assert_eq!(next_message(&mut orch_rx).await, info(2));

    // Nothing is delivered twice: the next message is a fresh one.
    server.send(key.public(), text(99));
    assert_eq!(next_message(&mut node_rx).await, text(99));

    // Acks drain both outboxes.
    tokio::time::timeout(Duration::from_secs(20), async {
        while server.pending(key.public()) > 0 || client.pending() > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("outboxes should drain once acked");
    run2.abort();
}

/// A node process restarts (new Client, same key): the orchestrator's queued and unacked
/// messages reach the new process, starting from its first message, with no gap.
#[tokio::test]
async fn node_restart_resets_the_stream() {
    let auth = TestAuth::default();
    let key = SecretKey::generate();
    auth.allowed.lock().unwrap().insert(key.public());
    let (_router, server, mut orch_rx, orch_addr) = start_orchestrator(auth).await;

    let (client, mut node_rx) = Client::new(orch_addr.clone());
    let ep = node_endpoint(&key).await;
    let (c, cf, epc) = (client.clone(), cfg(&orch_addr, None), ep.clone());
    let run = tokio::spawn(async move { c.run(&epc, cf).await });
    assert!(matches!(
        next(&mut orch_rx).await,
        Inbound::Connected { .. }
    ));
    for i in 0..5 {
        server.send(key.public(), text(i));
    }
    for i in 0..5 {
        assert_eq!(next_message(&mut node_rx).await, text(i));
    }
    // The node acks asynchronously, after handing a message on. Wait for the acks, so m0-m4
    // count as handled by the old process: anything still unacked when a node restarts is
    // rightly replayed to the new process, which would come before m5 here.
    tokio::time::timeout(Duration::from_secs(20), async {
        while server.pending(key.public()) > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the node acks what it received");
    run.abort();
    ep.close().await;
    // Queued while the node process is gone.
    server.send(key.public(), text(5));

    let (client2, mut node_rx2) = Client::new(orch_addr.clone());
    let ep2 = node_endpoint(&key).await;
    let (c, cf) = (client2.clone(), cfg(&orch_addr, None));
    let _run2 = tokio::spawn(async move { c.run(&ep2, cf).await });
    assert_eq!(next_message(&mut node_rx2).await, text(5));
    client2.send(info(7));
    assert_eq!(next_message(&mut orch_rx).await, info(7));
    server.send(key.public(), text(6));
    assert_eq!(next_message(&mut node_rx2).await, text(6));
}

/// The orchestrator restarts (new Server, same key and address): a running node renumbers its
/// queued messages for the new process instead of having them dropped as duplicates.
#[tokio::test]
async fn orchestrator_restart_resets_the_stream() {
    let auth = TestAuth::default();
    let key = SecretKey::generate();
    auth.allowed.lock().unwrap().insert(key.public());
    let orch_key = SecretKey::generate();
    let ep1 = bind_endpoint(
        orch_key.clone(),
        RelayChoice::Disabled,
        loopback(),
        vec![ALPN.to_vec()],
    )
    .await
    .unwrap();
    let orch_addr = direct_addr(&ep1);
    let (server1, mut rx1) = Server::new(auth.clone());
    let router1 = Router::builder(ep1).accept(ALPN, server1.clone()).spawn();

    let (client, mut node_rx) = Client::new(orch_addr.clone());
    let ep = node_endpoint(&key).await;
    let (c, cf, epc) = (client.clone(), cfg(&orch_addr, None), ep.clone());
    let run1 = tokio::spawn(async move { c.run(&epc, cf).await });
    for i in 0..3 {
        client.send(info(i));
    }
    for i in 0..3 {
        assert_eq!(next_message(&mut rx1).await, info(i));
    }
    server1.send(key.public(), text(1));
    assert_eq!(next_message(&mut node_rx).await, text(1));
    router1.shutdown().await.unwrap();

    run1.abort();

    // A new orchestrator process with the same identity. It binds a fresh port, because the old
    // socket lingers in this process; the node (same Client, so the same session state) redials.
    let ep2 = bind_endpoint(
        orch_key,
        RelayChoice::Disabled,
        loopback(),
        vec![ALPN.to_vec()],
    )
    .await
    .unwrap();
    let addr2 = direct_addr(&ep2);
    let (server2, mut rx2) = Server::new(auth);
    let _router2 = Router::builder(ep2).accept(ALPN, server2.clone()).spawn();
    let (c, cf) = (client.clone(), cfg(&addr2, None));
    let _run2 = tokio::spawn(async move { c.run(&ep, cf).await });
    client.send(info(10));
    assert_eq!(
        next_message(&mut rx2).await,
        info(10),
        "first message after the restart is delivered"
    );
    server2.send(key.public(), text(2));
    assert_eq!(
        next_message(&mut node_rx).await,
        text(2),
        "not mistaken for a duplicate of the old stream"
    );
}

/// Revoking a node drops its live connection; with its key no longer allowed, it stays out.
#[tokio::test]
async fn disconnect_drops_the_live_connection() {
    let auth = TestAuth::default();
    let key = SecretKey::generate();
    auth.allowed.lock().unwrap().insert(key.public());
    let (_router, server, mut orch_rx, orch_addr) = start_orchestrator(auth.clone()).await;
    let (client, _node_rx) = Client::new(orch_addr.clone());
    let ep = node_endpoint(&key).await;
    let (c, cf) = (client.clone(), cfg(&orch_addr, None));
    let run = tokio::spawn(async move { c.run(&ep, cf).await });
    assert!(matches!(
        next(&mut orch_rx).await,
        Inbound::Connected { .. }
    ));
    auth.allowed.lock().unwrap().clear();
    server.disconnect(key.public());
    assert!(matches!(
        next(&mut orch_rx).await,
        Inbound::Disconnected { .. }
    ));
    let res = tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("client should give up")
        .unwrap();
    assert!(res.is_err(), "the reconnect is refused");
    assert!(!server.is_connected(key.public()));
}
