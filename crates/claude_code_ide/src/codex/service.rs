#[cfg(unix)]
use super::unix as platform;
#[cfg(windows)]
use super::windows as platform;
use super::{
    protocol::{self, read_frame, write_frame},
    router::{Outgoing, Peer, Router},
};
use anyhow::{Result, bail};
use futures::channel::mpsc as foreground;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use tokio::{
    sync::{mpsc, mpsc::error::TrySendError, oneshot},
    task::JoinSet,
};

pub enum Event {
    Query {
        directory: PathBuf,
        discovery: bool,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Status {
        available: bool,
        error: Option<String>,
        successful_request: Option<SystemTime>,
    },
}
type Events = foreground::UnboundedSender<Event>;
/// The endpoint this process currently serves, if it owns the router. The quit
/// handler unlinks it synchronously because task abort is asynchronous and the
/// runtime may never be dropped before exit.
pub type OwnedEndpoint = Arc<Mutex<Option<platform::EndpointIdentity>>>;

pub async fn run(home: PathBuf, events: Events, owned: OwnedEndpoint) {
    loop {
        let result = session(&home, &events, &owned).await;
        if let Ok(mut owned) = owned.lock() {
            *owned = None;
        }
        let error = result.err().map(|e| format!("{e:#}"));
        if events
            .unbounded_send(Event::Status {
                available: false,
                error,
                successful_request: None,
            })
            .is_err()
        {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
async fn session(home: &std::path::Path, events: &Events, owned: &OwnedEndpoint) -> Result<()> {
    #[cfg(unix)]
    if !home.is_absolute() {
        bail!("CODEX_HOME must be an absolute path");
    }
    let (owner, mut stream, endpoint) =
        tokio::time::timeout(protocol::REQUEST_BUDGET, platform::connect_or_bind(home)).await??;
    // JoinSet aborts the router and its connections when this session ends.
    let mut router = JoinSet::new();
    if let Some(listener) = owner {
        if let Ok(mut owned) = owned.lock() {
            *owned = Some(listener.identity());
        }
        router.spawn(serve_router(listener));
    }
    let provider = async {
        let initialize = json!({"type":"request", "requestId":uuid::Uuid::new_v4().to_string(),
        "sourceClientId":"initializing-client", "method":"initialize", "version":0, "params":{"clientType":"zed"}});
        write_frame(&mut stream, &initialize).await?;
        let client = tokio::time::timeout(protocol::REQUEST_BUDGET, async {
            loop {
                let message = read_frame(&mut stream).await?;
                if message["type"] == "response" && message["requestId"] == initialize["requestId"]
                {
                    if message["resultType"] != "success" || message["method"] != "initialize" {
                        bail!("incompatible Codex IPC router");
                    }
                    return message["result"]["clientId"]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow::anyhow!("missing IPC client ID"));
                }
            }
        })
        .await??;
        events.unbounded_send(Event::Status {
            available: true,
            error: None,
            successful_request: None,
        })?;
        loop {
            let message = read_frame(&mut stream).await?;
            match message["type"].as_str() {
                Some("client-discovery-request") => {
                    let request = &message["request"];
                    let can_handle = if protocol::context_request(request) {
                        match query(request, true, events).await {
                            Ok(_) => true,
                            Err(error) => {
                                if error != "no-client-found" {
                                    events.unbounded_send(Event::Status {
                                        available: true,
                                        error: Some(error),
                                        successful_request: None,
                                    })?;
                                }
                                false
                            }
                        }
                    } else {
                        false
                    };
                    write_frame(&mut stream, &json!({"type":"client-discovery-response", "requestId":message["requestId"], "response":{"canHandle":can_handle}})).await?;
                }
                Some("request") => {
                    let result = if message["method"] != "ide-context" {
                        Err("no-handler-for-request".into())
                    } else if !protocol::context_request(&message) {
                        Err("request-version-mismatch".into())
                    } else {
                        query(&message, false, events).await
                    };
                    let (response, error) = match result {
                        Ok(context) => (
                            protocol::success(&message, &client, json!({"ideContext":context})),
                            None,
                        ),
                        Err(error) => (protocol::error(&message, &error), Some(error)),
                    };
                    write_frame(&mut stream, &response).await?;
                    let successful_request = if error.is_none() {
                        Some(SystemTime::now())
                    } else {
                        None
                    };
                    events.unbounded_send(Event::Status {
                        available: true,
                        error,
                        successful_request,
                    })?;
                }
                Some("broadcast") | Some("response") => {}
                _ => bail!("unexpected Codex IPC message"),
            }
        }
    };
    #[cfg(unix)]
    {
        let watch_endpoint = async {
            let mut timer = tokio::time::interval(Duration::from_secs(1));
            loop {
                timer.tick().await;
                if !endpoint.is_current() {
                    bail!("Codex IPC endpoint was replaced");
                }
            }
        };
        tokio::select! { result = provider => result, result = watch_endpoint => result }
    }
    #[cfg(windows)]
    {
        let _ = endpoint;
        provider.await
    }
}

async fn query(request: &Value, discovery: bool, events: &Events) -> Result<Value, String> {
    let directory = request["params"]["workspaceRoot"]
        .as_str()
        .ok_or("invalid-workspace-root")?
        .into();
    let (reply, response) = oneshot::channel();
    events
        .unbounded_send(Event::Query {
            directory,
            discovery,
            reply,
        })
        .map_err(|_| "editor-unavailable")?;
    tokio::time::timeout(Duration::from_secs(2), response)
        .await
        .map_err(|_| "request-timeout")?
        .map_err(|_| "editor-unavailable")?
}

enum SocketEvent {
    Message(Peer, Value),
    Closed(Peer),
}
async fn connection(
    peer: Peer,
    stream: platform::Stream,
    events: mpsc::Sender<SocketEvent>,
    mut outbound: mpsc::Receiver<Value>,
) {
    let (mut reader, mut writer) = tokio::io::split(stream);
    // Separate futures keep a partially read frame alive while a write completes.
    let read = async {
        loop {
            let value = read_frame(&mut reader).await?;
            events.send(SocketEvent::Message(peer, value)).await?;
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    let write = async {
        while let Some(value) = outbound.recv().await {
            write_frame(&mut writer, &value).await?;
        }
        anyhow::Ok(())
    };
    tokio::select! { _ = read => {}, _ = write => {} }
    let _ = events.send(SocketEvent::Closed(peer)).await;
}
async fn serve_router(mut listener: platform::Listener) -> Result<()> {
    let mut router = Router::default();
    let mut peers = HashMap::new();
    let (tx, mut rx) = mpsc::channel(128);
    let mut tasks = JoinSet::new();
    let mut next_peer = 0;
    let mut timer = tokio::time::interval(Duration::from_millis(50));
    let mut endpoint_check = tokio::time::interval(Duration::from_secs(1));
    loop {
        let outgoing = tokio::select! {
            accepted = listener.accept(), if peers.len() < 64 => {
                let stream = match accepted {
                    Ok(stream) => stream,
                    Err(error) => {
                        log::warn!("Codex IPC accept: {error:#}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                next_peer += 1;
                // Bounded so a stalled pipe cannot hold unbounded frames. Room
                // for one discovery fan-out per pending request plus a burst of
                // status broadcasts before the writer task drains.
                let (outbound, receiver) = mpsc::channel(64);
                peers.insert(next_peer, outbound);
                tasks.spawn(connection(next_peer, stream, tx.clone(), receiver));
                vec![]
            },
            event = rx.recv() => match event {
                Some(SocketEvent::Message(peer, value)) if peers.contains_key(&peer) => router.message(peer, value),
                Some(SocketEvent::Closed(peer)) => { peers.remove(&peer); router.disconnect(peer) },
                _ => vec![],
            },
            _ = timer.tick(), if router.has_pending() => router.expire(std::time::Instant::now()),
            _ = endpoint_check.tick() => {
                // A competing Unix router can unlink our pathname without
                // closing existing streams. Retire this orphaned listener so
                // our provider and its other clients reconnect to the winner.
                #[cfg(unix)]
                if !listener.is_current() { bail!("Codex IPC endpoint was replaced"); }
                vec![]
            },
            _ = tasks.join_next(), if !tasks.is_empty() => vec![],
        };
        dispatch(outgoing, &mut peers, &mut router);
    }
}
fn dispatch(
    outgoing: Outgoing,
    peers: &mut HashMap<Peer, mpsc::Sender<Value>>,
    router: &mut Router,
) {
    // Router output is ordered; a disconnect cascade queues behind it.
    let mut outgoing: VecDeque<_> = outgoing.into();
    while let Some((peer, message)) = outgoing.pop_front() {
        let Some(sender) = peers.get(&peer) else {
            continue;
        };
        match sender.try_send(message) {
            Ok(()) => {}
            // Status broadcasts are advisory. A peer that has not drained its
            // queue misses one rather than losing a connection that is still
            // serving requests.
            Err(TrySendError::Full(message)) if message["type"] == "broadcast" => {
                log::debug!("Codex IPC peer {peer} is behind; dropped a broadcast");
            }
            // A recipient that cannot take a request or response it is party to
            // has stalled, and a closed one is already gone. Neither may
            // backpressure the application, so its queue is closed, which tears
            // down the pipe and fails its requests now rather than at the
            // deadline.
            Err(TrySendError::Full(_) | TrySendError::Closed(_)) => {
                peers.remove(&peer);
                outgoing.extend(router.disconnect(peer));
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    async fn request(home: &std::path::Path) -> Value {
        let mut stream = platform::connect(&platform::endpoint(home)).await.unwrap();
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/codex/exchanges.json"))
                .unwrap();
        write_frame(&mut stream, &fixture["request"]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), read_frame(&mut stream))
            .await
            .unwrap()
            .unwrap()
    }
    async fn ready(status: &mut mpsc::UnboundedReceiver<bool>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while status.recv().await != Some(true) {}
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn rejoins_router_when_socket_path_is_replaced() {
        replaced_socket(true).await;
    }

    #[tokio::test]
    async fn rejoins_when_external_router_is_still_alive_but_unlinked() {
        replaced_socket(false).await;
    }

    async fn replaced_socket(zed_owned: bool) {
        let home = std::env::temp_dir().join(format!(
            "zr-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        let external = if zed_owned {
            None
        } else {
            // An external router need not notice that its endpoint was unlinked.
            // Keep its provider connection open to prove recovery does not rely
            // on EOF or the old router process exiting.
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(home.join("ipc")).unwrap();
            std::fs::set_permissions(home.join("ipc"), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let listener = tokio::net::UnixListener::bind(platform::endpoint(&home)).unwrap();
            Some(tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let initialize = read_frame(&mut stream).await.unwrap();
                write_frame(
                    &mut stream,
                    &protocol::success(&initialize, "external", json!({"clientId":"external"})),
                )
                .await
                .unwrap();
                std::future::pending::<()>().await;
                drop(stream);
            }))
        };
        let (events, mut incoming) = foreground::unbounded();
        let (state_tx, mut state_rx) = mpsc::unbounded_channel();
        let foreground_task = tokio::spawn(async move {
            while let Some(event) = incoming.next().await {
                match event {
                    Event::Query { reply, .. } => {
                        let _ = reply.send(Ok(json!({"activeFile":null,"openTabs":[]})));
                    }
                    Event::Status { available, .. } => {
                        let _ = state_tx.send(available);
                    }
                }
            }
        });
        let provider = tokio::spawn(run(home.clone(), events, OwnedEndpoint::default()));
        ready(&mut state_rx).await;
        if zed_owned {
            assert_eq!(request(&home).await["resultType"], "success");
        }
        while state_rx.try_recv().is_ok() {}

        // Reproduce the official manager's startup race: its earlier connect
        // probe failed, but by the time it unlinks/binds, Zed owns this path.
        std::fs::remove_file(platform::endpoint(&home)).unwrap();
        let (replacement, stream, _) = platform::connect_or_bind(&home).await.unwrap();
        drop(stream);
        let replacement = tokio::spawn(serve_router(replacement.unwrap()));
        let recovered = tokio::time::timeout(Duration::from_secs(5), async {
            while state_rx.recv().await != Some(false) {}
            ready(&mut state_rx).await;
        })
        .await;
        if recovered.is_ok() {
            assert_eq!(request(&home).await["resultType"], "success");
        }
        if let Some(external) = external {
            assert!(!external.is_finished());
            external.abort();
            let _ = external.await;
        }
        provider.abort();
        let _ = provider.await;
        replacement.abort();
        let _ = replacement.await;
        foreground_task.abort();
        let _ = foreground_task.await;
        tokio::task::yield_now().await;
        std::fs::remove_dir_all(home).unwrap();
        assert!(
            recovered.is_ok(),
            "provider remained registered with an unreachable router"
        );
    }

    #[tokio::test]
    async fn fresh_context_short_lived_clients_and_router_owner_exit() {
        let home = std::env::temp_dir().join(format!(
            "zs-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        // Existing router first: provider must join it, then recover when it exits.
        let (owner, stream, _) = platform::connect_or_bind(&home).await.unwrap();
        drop(stream);
        let owner = tokio::spawn(serve_router(owner.unwrap()));
        let (events, mut incoming) = foreground::unbounded();
        let (state_tx, mut state_rx) = mpsc::unbounded_channel();
        let version = Arc::new(AtomicUsize::new(1));
        let foreground_version = version.clone();
        let foreground_task = tokio::spawn(async move {
            while let Some(event) = incoming.next().await {
                match event {
                    Event::Query {
                        discovery, reply, ..
                    } => {
                        let context = if discovery {
                            Value::Null
                        } else {
                            json!({"activeFile":null,"openTabs":[],"revision":foreground_version.load(Ordering::SeqCst)})
                        };
                        let _ = reply.send(Ok(context));
                    }
                    Event::Status { available, .. } => {
                        let _ = state_tx.send(available);
                    }
                }
            }
        });
        let provider = tokio::spawn(run(home.clone(), events, OwnedEndpoint::default()));
        ready(&mut state_rx).await;
        assert_eq!(request(&home).await["result"]["ideContext"]["revision"], 1);
        version.store(2, Ordering::SeqCst);
        assert_eq!(request(&home).await["result"]["ideContext"]["revision"], 2);
        // Drain successful request statuses before waiting for actual recovery.
        while state_rx.try_recv().is_ok() {}
        owner.abort();
        let _ = owner.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while state_rx.recv().await != Some(false) {}
        })
        .await
        .unwrap();
        ready(&mut state_rx).await;
        assert_eq!(request(&home).await["resultType"], "success");
        provider.abort();
        let _ = provider.await;
        foreground_task.abort();
        let _ = foreground_task.await;
        tokio::task::yield_now().await;
        std::fs::remove_dir_all(home).unwrap();
    }
}
