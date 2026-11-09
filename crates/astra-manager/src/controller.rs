//! Authenticated outbound agent channels. Requests are routed once and never replayed.
use anyhow::{Context, Result};
use astra_management::{config::ControllerConfig, v2::*};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    io::BufReader,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::{self, Message};
#[cfg(test)]
#[path = "test/controller_test.rs"]
mod tests;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum Frame {
    Hello(AgentHello),
    Welcome(Hello),
    Request(Request),
    Response(Response),
    Heartbeat,
}
#[derive(Serialize, Deserialize)]
struct AgentHello {
    version: u16,
    build_id: String,
    agent: Agent,
    capabilities: Vec<String>,
    instances: Option<Vec<Instance>>,
    tasks: Vec<Task>,
    event_cursor: String,
}
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Response>>>>;
struct Peer {
    sender: mpsc::Sender<Request>,
    pending: Pending,
    connection: u64,
    revoked: Arc<tokio::sync::Notify>,
}
pub struct Controller {
    config: ControllerConfig,
    credentials: Mutex<BTreeMap<String, String>>,
    peers: Mutex<HashMap<String, Peer>>,
    known: Mutex<HashMap<String, Agent>>,
    sequence: AtomicU64,
    epoch: String,
    index: Option<crate::database::Database>,
}
impl Controller {
    pub(crate) async fn database_closed(&self) -> anyhow::Error {
        match &self.index {
            Some(database) => database.closed().await,
            None => std::future::pending().await,
        }
    }
    pub fn new(config: ControllerConfig) -> Arc<Self> {
        Arc::new(Self {
            credentials: Mutex::new(config.agent_token_hashes.clone()),
            config,
            peers: Mutex::new(HashMap::new()),
            known: Mutex::new(HashMap::new()),
            sequence: AtomicU64::new(1),
            epoch: uuid::Uuid::new_v4().to_string(),
            index: None,
        })
    }
    pub async fn open(
        config: ControllerConfig,
        state_dir: &std::path::Path,
        event_limit: usize,
    ) -> Result<Arc<Self>> {
        crate::private_dir(state_dir)?;
        let path = state_dir.join("controller.sqlite");
        let index = crate::database::Database::open(path.clone(), event_limit)?;
        astra_platform::platform::set_private(&path, 0o600)?;
        let mut known = HashMap::new();
        for mut agent in index.remembered_agents().await? {
            agent.online = false;
            known.insert(agent.id.clone(), agent);
        }
        let mut controller = Self::new(config);
        let inner = Arc::get_mut(&mut controller).expect("new controller has single owner");
        inner.index = Some(index);
        inner.known = Mutex::new(known);
        Ok(controller)
    }
    fn hello(&self) -> Hello {
        Hello {
            version: VERSION,
            role: "controller".into(),
            epoch: self.epoch.clone(),
            build_id: env!("CARGO_PKG_VERSION").into(),
            capabilities: vec![
                "agent_routing".into(),
                "no_replay".into(),
                "credential_revocation".into(),
                "durable_index".into(),
            ],
        }
    }
    /// Credential changes apply to new handshakes and immediately revoke existing channels.
    pub fn replace_credentials(&self, credentials: BTreeMap<String, String>) {
        let mut current = self.credentials.lock().unwrap();
        let mut peers = self.peers.lock().unwrap();
        let revoked: Vec<_> = peers
            .keys()
            .filter(|id| credentials.get(*id) != current.get(*id))
            .cloned()
            .collect();
        for id in revoked {
            if let Some(peer) = peers.remove(&id) {
                peer.revoked.notify_one();
                peer.pending.lock().unwrap().clear();
                if let Some(agent) = self.known.lock().unwrap().get_mut(&id) {
                    agent.online = false;
                }
            }
        }
        *current = credentials;
    }
    pub async fn handle(&self, mut request: Request) -> Response {
        let original = request.id.clone();
        if request.version != VERSION {
            return Response::error(
                original,
                ErrorCode::VersionMismatch,
                "Management v2 required",
            );
        }
        if matches!(request.body, RequestBody::Local(_)) {
            return Response::error(
                original,
                ErrorCode::RequiresLocalAction,
                "Local elevation cannot be forwarded through a controller",
            );
        }
        if matches!(request.body, RequestBody::Query(Query::Hello)) && request.agent_id.is_none() {
            return Response::success(original, ResponseBody::Hello(self.hello()));
        }
        if matches!(request.body, RequestBody::Query(Query::Agents)) {
            let mut known = self.known.lock().unwrap().clone();
            for id in self.credentials.lock().unwrap().keys() {
                known.entry(id.clone()).or_insert_with(|| Agent {
                    id: id.clone(),
                    name: id.clone(),
                    boot_id: String::new(),
                    online: false,
                    last_seen_ms: "0".into(),
                });
            }
            return Response::success(
                original,
                ResponseBody::Agents(known.into_values().collect()),
            );
        }
        let Some(agent) = request.agent_id.clone() else {
            return Response::error(
                original,
                ErrorCode::InvalidRequest,
                "agent_id is required at the controller",
            );
        };
        let peer = self
            .peers
            .lock()
            .unwrap()
            .get(&agent)
            .map(|p| (p.sender.clone(), p.pending.clone()));
        let Some((sender, pending)) = peer else {
            return Response::error(
                original,
                ErrorCode::Offline,
                "Agent is offline; no command was queued",
            );
        };
        let operation = match &request.body {
            RequestBody::Execute { operation_id, .. }
            | RequestBody::Drafts(DraftCommand::Save { operation_id, .. }) => {
                Some(operation_id.clone())
            }
            _ => None,
        };
        let observed = match &request.body {
            RequestBody::Query(
                Query::Task { operation_id } | Query::NetworkReport { operation_id, .. },
            ) => Some(operation_id.clone()),
            _ => None,
        };
        if let (Some(index), Some(operation)) = (&self.index, &operation) {
            if let Err(error) = index
                .route(operation, &agent, &original, "forwarding")
                .await
            {
                return crate::error::response(original, error.context("Persist routing identity"));
            }
        }
        let route_id = format!("route-{}", self.sequence.fetch_add(1, Ordering::Relaxed));
        request.id = route_id.clone();
        let (reply, receiver) = oneshot::channel();
        pending.lock().unwrap().insert(route_id.clone(), reply);
        if sender.try_send(request).is_err() {
            pending.lock().unwrap().remove(&route_id);
            return Response::error(
                original,
                ErrorCode::Offline,
                "Agent channel unavailable or full; request was not queued",
            );
        }
        let response = match tokio::time::timeout(Duration::from_secs(2400), receiver).await {
            Ok(Ok(mut response)) => {
                response.id = original;
                response
            }
            _ => {
                pending.lock().unwrap().remove(&route_id);
                Response::error(original,ErrorCode::OutcomeUnknown,"Agent disconnected or reply timed out; query the operation ID after reconnecting")
            }
        };
        if let (Some(index), Some(operation_id)) =
            (&self.index, operation.as_ref().or(observed.as_ref()))
        {
            let state = match &response.result {
                Err(ApiError {
                    code: ErrorCode::OutcomeUnknown,
                    ..
                }) => "outcome_unknown",
                Ok(ResponseBody::Task(task)) if matches!(task.state, TaskState::Running) => {
                    "accepted"
                }
                Ok(ResponseBody::Task(task)) if task.finished_ms.is_some() => "completed",
                Ok(ResponseBody::NetworkReport(report)) if report.state.terminal() => "completed",
                Ok(ResponseBody::NetworkReport(_)) => "accepted",
                _ => "responded",
            };
            // A failed observation must not replace the last known execution state.
            if operation.is_some() || response.result.is_ok() {
                let _ = index.route(operation_id, &agent, &response.id, state).await;
            }
        }
        response
    }
    pub async fn listen(self: Arc<Self>) -> Result<()> {
        let certs = rustls_pemfile::certs(&mut BufReader::new(std::fs::File::open(
            &self.config.tls_cert,
        )?))
        .collect::<std::io::Result<Vec<_>>>()?;
        let key = rustls_pemfile::private_key(&mut BufReader::new(std::fs::File::open(
            &self.config.tls_key,
        )?))?
        .context("TLS private key missing")?;
        let tls = tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)?;
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let listener = tokio::net::TcpListener::bind(self.config.listen).await?;
        let capacity = Arc::new(tokio::sync::Semaphore::new(128));
        loop {
            let (stream, _) = listener.accept().await?;
            let Ok(permit) = capacity.clone().try_acquire_owned() else {
                continue;
            };
            let controller = self.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(error) = controller.accept(stream, acceptor).await {
                    eprintln!("Agent channel closed: {error}");
                }
            });
        }
    }
    #[allow(clippy::result_large_err)] // The WebSocket callback contract owns an HTTP rejection response.
    async fn accept(
        self: Arc<Self>,
        stream: tokio::net::TcpStream,
        acceptor: tokio_rustls::TlsAcceptor,
    ) -> Result<()> {
        let stream =
            tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream)).await??;
        let authenticated = Arc::new(Mutex::new(None::<(String, String)>));
        let selected = authenticated.clone();
        let authentication = self.clone();
        let mut ws_config = tungstenite::protocol::WebSocketConfig::default();
        ws_config.max_message_size = Some(MAX_FRAME_BYTES);
        ws_config.max_frame_size = Some(MAX_FRAME_BYTES);
        let socket = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::accept_hdr_async_with_config(
                stream,
                move |request: &tungstenite::handshake::server::Request,
                      response: tungstenite::handshake::server::Response| {
                    use subtle::ConstantTimeEq;
                    let id = request
                        .headers()
                        .get("x-astra-agent-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let token = request
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .unwrap_or("");
                    let valid = request.uri().path() == "/agents/v2"
                        && authentication
                            .credentials
                            .lock()
                            .unwrap()
                            .get(id)
                            .is_some_and(|expected| {
                                use sha2::Digest;
                                let actual =
                                    format!("{:x}", sha2::Sha256::digest(token.as_bytes()));
                                token.len() >= 32
                                    && bool::from(expected.as_bytes().ct_eq(actual.as_bytes()))
                            });
                    if !valid {
                        return Err(tungstenite::http::Response::builder()
                            .status(401)
                            .body(Some("Agent authentication required".into()))
                            .unwrap());
                    }
                    use sha2::Digest;
                    *selected.lock().unwrap() = Some((
                        id.to_owned(),
                        format!("{:x}", sha2::Sha256::digest(token.as_bytes())),
                    ));
                    Ok(response)
                },
                Some(ws_config),
            ),
        )
        .await??;
        let (mut writer, mut reader) = socket.split();
        let hello = tokio::time::timeout(Duration::from_secs(10), reader.next())
            .await?
            .context("Agent greeting missing")??;
        let Frame::Hello(hello) = serde_json::from_str(hello.to_text()?)? else {
            anyhow::bail!("Expected agent greeting");
        };
        anyhow::ensure!(hello.version == VERSION, "Agent protocol version mismatch");
        let mut identity = hello.agent.clone();
        anyhow::ensure!(
            authenticated
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|(id, _)| id == &identity.id),
            "Agent identity mismatch"
        );
        identity.online = true;
        identity.last_seen_ms = crate::store::now();
        let id = identity.id.clone();
        let connection = self.sequence.fetch_add(1, Ordering::Relaxed);
        let pending: Pending = Default::default();
        let revoked = Arc::new(tokio::sync::Notify::new());
        let (sender, mut requests) = mpsc::channel(32);
        {
            let credentials = self.credentials.lock().unwrap();
            anyhow::ensure!(
                authenticated
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|(id, digest)| credentials.get(id) == Some(digest)),
                "Agent credential changed during handshake"
            );
            let mut peers = self.peers.lock().unwrap();
            anyhow::ensure!(!peers.contains_key(&id), "Agent already connected");
            peers.insert(
                id.clone(),
                Peer {
                    sender,
                    pending: pending.clone(),
                    connection,
                    revoked: revoked.clone(),
                },
            );
        }
        self.known
            .lock()
            .unwrap()
            .insert(id.clone(), identity.clone());
        let outcome:Result<()>=async {
            if let Some(index)=&self.index{index.remember_agent(identity).await?;index.snapshot(&id,&serde_json::to_string(&hello)?).await?;}
            writer.send(Message::Text(serde_json::to_string(&Frame::Welcome(self.hello()))?.into())).await?;
            let mut heartbeat=tokio::time::interval(Duration::from_secs(15));
            let mut last_seen=tokio::time::Instant::now();
            loop {tokio::select! {
                biased;
                _=revoked.notified()=>{let _=writer.send(Message::Close(None)).await;break;}
                request=requests.recv()=>{let Some(request)=request else{break;};writer.send(Message::Text(serde_json::to_string(&Frame::Request(request))?.into())).await?;}
                incoming=reader.next()=>{
                    let message=incoming.context("Agent disconnected")?;let message=message?;
                    last_seen=tokio::time::Instant::now();
                    match message {
                        Message::Text(text)=>match serde_json::from_str::<Frame>(&text)? {
                            Frame::Response(response)=>{if let Some(reply)=pending.lock().unwrap().remove(&response.id){let _=reply.send(response);}}
                            Frame::Heartbeat=>{if let Some(agent)=self.known.lock().unwrap().get_mut(&id){agent.last_seen_ms=crate::store::now();}}
                            _=>anyhow::bail!("Unexpected agent message"),
                        },
                        Message::Ping(data)=>writer.send(Message::Pong(data)).await?,Message::Pong(_)=>{},Message::Close(_)=>break,_=>anyhow::bail!("Unsupported agent frame"),
                    }
                }
                _=heartbeat.tick()=>{anyhow::ensure!(last_seen.elapsed()<Duration::from_secs(45),"Agent heartbeat expired");writer.send(Message::Ping(Vec::new().into())).await?;},
            }} Ok(())
        }.await;
        let identity = {
            let mut peers = self.peers.lock().unwrap();
            if peers.get(&id).is_some_and(|p| p.connection == connection) {
                peers.remove(&id);
                let mut known = self.known.lock().unwrap();
                known.get_mut(&id).map(|identity| {
                    identity.online = false;
                    identity.clone()
                })
            } else {
                None
            }
        };
        if let (Some(index), Some(identity)) = (&self.index, identity) {
            let _ = index.remember_agent(identity).await;
        }
        pending.lock().unwrap().clear(); // Dropping callers reports uncertainty; no replay queue exists.
        outcome
    }
}

pub async fn uplink(agent: Arc<crate::agent::AgentService>) -> Result<()> {
    let config = &agent.config.agent;
    let Some(url) = &config.controller_url else {
        return Ok(());
    };
    let mut delay = 250;
    loop {
        let result = connect_configured(agent.clone(), url).await;
        if let Err(error) = result {
            eprintln!("Controller channel unavailable: {error}; reconnecting without replay");
        }
        tokio::time::sleep(Duration::from_millis(delay)).await;
        delay = (delay * 2).min(5000);
    }
}
async fn connect_configured(agent: Arc<crate::agent::AgentService>, url: &str) -> Result<()> {
    let config = &agent.config.agent;
    let token = std::fs::read_to_string(
        config
            .token_file
            .as_ref()
            .context("agent token_file missing")?,
    )?
    .trim()
    .to_owned();
    anyhow::ensure!(!token.is_empty(), "agent token is empty");
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = &config.ca_certificate {
        for cert in rustls_pemfile::certs(&mut BufReader::new(std::fs::File::open(path)?)) {
            roots.add(cert?)?;
        }
    }
    let tls = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_tungstenite::Connector::Rustls(Arc::new(tls));
    connect_once(agent, url, &token, connector).await
}
async fn connect_once(
    agent: Arc<crate::agent::AgentService>,
    url: &str,
    token: &str,
    connector: tokio_tungstenite::Connector,
) -> Result<()> {
    use tungstenite::client::IntoClientRequest;
    let mut request = url.into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse()?);
    request
        .headers_mut()
        .insert("x-astra-agent-id", agent.identity.id.parse()?);
    let mut config = tungstenite::protocol::WebSocketConfig::default();
    config.max_message_size = Some(MAX_FRAME_BYTES);
    config.max_frame_size = Some(MAX_FRAME_BYTES);
    let (socket, _) = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async_tls_with_config(
            request,
            Some(config),
            false,
            Some(connector),
        ),
    )
    .await??;
    let (mut writer, mut reader) = socket.split();
    let instances = match agent
        .handle(Request::new(
            "reconnect-instances",
            RequestBody::Query(Query::Instances),
        ))
        .await
        .result
    {
        Ok(ResponseBody::Instances(items)) => Some(items),
        _ => None,
    };
    let tasks = match agent
        .handle(Request::new(
            "reconnect-tasks",
            RequestBody::Query(Query::Tasks { limit: Some(128) }),
        ))
        .await
        .result
    {
        Ok(ResponseBody::Tasks(items)) => items,
        _ => vec![],
    };
    let event_cursor = match agent
        .handle(Request::new(
            "reconnect-events",
            RequestBody::Query(Query::Events {
                after: None,
                limit: Some(1),
            }),
        ))
        .await
        .result
    {
        Ok(ResponseBody::Events(batch)) => batch.cursor,
        _ => "0".into(),
    };
    writer
        .send(Message::Text(
            serde_json::to_string(&Frame::Hello(AgentHello {
                version: VERSION,
                build_id: env!("CARGO_PKG_VERSION").into(),
                agent: agent.identity.clone(),
                capabilities: vec![
                    "client".into(),
                    "server".into(),
                    "documents".into(),
                    "artifacts".into(),
                    "durable_tasks".into(),
                ],
                instances,
                tasks,
                event_cursor,
            }))?
            .into(),
        ))
        .await?;
    let welcome = tokio::time::timeout(Duration::from_secs(10), reader.next())
        .await?
        .context("Controller welcome missing")??;
    let Frame::Welcome(welcome) = serde_json::from_str(welcome.to_text()?)? else {
        anyhow::bail!("Expected controller welcome")
    };
    anyhow::ensure!(
        welcome.version == VERSION && !welcome.epoch.is_empty(),
        "Controller protocol version mismatch"
    );
    let (sender, mut outgoing) = mpsc::channel::<Response>(32);
    let capacity = Arc::new(tokio::sync::Semaphore::new(32));
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    let mut last_seen = tokio::time::Instant::now();
    loop {
        tokio::select! {
            response=outgoing.recv()=>{if let Some(response)=response{writer.send(Message::Text(serde_json::to_string(&Frame::Response(response))?.into())).await?;}}
        message=reader.next()=>{
            let message=message.context("Controller disconnected")??;
            last_seen=tokio::time::Instant::now();
            match message {
                    Message::Text(text)=>{
                        let Frame::Request(request)=serde_json::from_str(&text)? else{anyhow::bail!("Unexpected controller frame");};
                        let permit=capacity.clone().try_acquire_owned().context("agent request capacity exceeded")?;
                        let agent=agent.clone();let sender=sender.clone();
                        // Execution outlives the network channel; durable operation IDs reconcile lost replies.
                        tokio::spawn(async move{let _permit=permit;let response=agent.handle_remote(request).await;let _=sender.send(response).await;});
                    }
                    Message::Ping(data)=>writer.send(Message::Pong(data)).await?,Message::Pong(_)=>{},Message::Close(_)=>return Ok(()),_=>anyhow::bail!("Unsupported controller frame"),
                }
            }
        _=heartbeat.tick()=>{anyhow::ensure!(last_seen.elapsed()<Duration::from_secs(45),"Controller heartbeat expired");writer.send(Message::Text(serde_json::to_string(&Frame::Heartbeat)?.into())).await?;},
        }
    }
}

#[cfg(test)]
#[path = "test/network_controller_test.rs"]
mod network_tests;
