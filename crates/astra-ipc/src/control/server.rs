use super::*;
use crate::{progress, session::Event};
use futures_util::{stream::FuturesUnordered, StreamExt};
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::{
    sync::{mpsc, oneshot, Semaphore},
    task::JoinSet,
};

struct Work {
    command: Command,
    reply: oneshot::Sender<Response>,
    progress: progress::Sender,
}

const OPERATION_DEADLINE: Duration = Duration::from_secs(60);
const MUTATION_LIMIT: usize = 16;

fn accept_signal_reload(in_flight: usize) -> bool {
    if in_flight >= MUTATION_LIMIT {
        tracing::warn!(
            trigger = "sighup",
            in_flight,
            limit = MUTATION_LIMIT,
            "Configuration reload skipped: control queue is full"
        );
        false
    } else {
        true
    }
}

async fn signal_reload(future: impl Future<Output = Result<(), String>>, deadline: Duration) {
    tokio::pin!(future);
    tracing::info!(trigger = "sighup", "Configuration reload accepted");
    let result = match timeout(deadline, future.as_mut()).await {
        Ok(result) => result,
        Err(_) => {
            // The deadline is an observation limit, not a transaction abort.
            // Keep polling the same accepted operation until it finishes.
            tracing::warn!(
                trigger = "sighup",
                deadline_secs = deadline.as_secs(),
                "Configuration reload deadline exceeded; reload is still running"
            );
            future.await
        }
    };
    match result {
        Ok(()) => tracing::info!(trigger = "sighup", "Configuration reload completed"),
        Err(error) => tracing::warn!(trigger = "sighup", %error, "Configuration reload rejected"),
    }
}

fn response_error(response: &Response) -> Option<&str> {
    match response {
        Response::Status(status) => status.error.as_deref(),
        Response::Config(Err(error))
        | Response::Explanation(Err(error))
        | Response::Runtime(Err(error))
        | Response::Monitor(Err(error))
        | Response::RuntimeTelemetry(Err(error)) => Some(error),
        _ => None,
    }
}

async fn finish_request(
    future: impl Future<Output = Response>,
    reply: oneshot::Sender<Response>,
    kind: u8,
    baseline: Status,
    mutation: bool,
    deadline: Duration,
) {
    tokio::pin!(future);
    match timeout(deadline, future.as_mut()).await {
        Ok(response) => {
            let _ = reply.send(response);
        }
        Err(_) if mutation => {
            let _ = reply.send(failure(kind,
                "Core operation deadline exceeded; outcome unknown and operation is still running. Query current state before retrying".into(), baseline));
            tracing::warn!(
                deadline_secs = deadline.as_secs(),
                "Accepted core mutation deadline exceeded; operation retained without replay"
            );
            let response = future.await;
            if let Some(error) = response_error(&response) {
                tracing::warn!(%error, "Core mutation finished with an error after its response deadline");
            } else {
                tracing::info!("Core mutation completed after its response deadline");
            }
        }
        Err(_) => {
            let _ = reply.send(failure(kind, "Core query timed out".into(), baseline));
        }
    }
}

pub(super) async fn run<F, Fut, G, SF, H, EF, U, UF, R, RF>(
    control: &Control,
    mut reload: F,
    mut snapshot: G,
    mut explain: H,
    mut update: U,
    mut runtime: R,
) -> io::Result<()>
where
    F: FnMut(Option<PathBuf>) -> Fut,
    Fut: Future<Output = Result<(), String>> + Send,
    G: FnMut() -> SF,
    SF: Future<Output = Result<ConfigSnapshot, String>> + Send,
    H: FnMut(crate::RouteRequest) -> EF,
    EF: Future<Output = Result<RouteExplanation, String>> + Send,
    U: FnMut() -> UF,
    UF: Future<Output = Result<(), String>> + Send,
    R: FnMut(astra_model::runtime::Command) -> RF,
    RF: Future<Output = Result<astra_model::runtime::Snapshot, String>> + Send,
{
    let (sender, mut commands) = mpsc::channel::<Work>(64);
    let (stopped, mut stop) = mpsc::channel::<()>(1);
    let permits = Arc::new(Semaphore::new(64));
    let mut connections = JoinSet::new();
    let mut jobs: FuturesUnordered<Pin<Box<dyn Future<Output = ()> + Send + '_>>> =
        FuturesUnordered::new();
    let signal = crate::shutdown_signal();
    tokio::pin!(signal);
    let mut hup = astra_platform::platform::ReloadSignal::new()?;
    loop {
        let accept = async {
            match &control.listener {
                Some(listener) => listener.accept().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            error = control.candidate.wait_owner_loss() => return Err(error),
            result = &mut signal => { result?; break; }
            _ = stop.recv() => break,
            _ = hup.recv() => {
                if accept_signal_reload(jobs.len()) {
                    let future = reload(None);
                    jobs.push(Box::pin(signal_reload(future, OPERATION_DEADLINE)));
                }
            }
            result = accept => {
                let (stream, _) = match result { Ok(value)=>value, Err(error) if astra_platform::supervision::transient_io(&error)=>{tokio::time::sleep(Duration::from_millis(100)).await;continue;}, Err(error)=>return Err(error) };
                if let Ok(permit) = permits.clone().try_acquire_owned() {
                    let (sender, stopped) = (sender.clone(), stopped.clone());
                    connections.spawn(async move { let _permit = permit; let _ = serve(stream, sender, stopped).await; });
                }
            }
            Some(work) = commands.recv() => {
                if work.reply.is_closed() { continue; }
                let status = || Status { api_version: crate::API_VERSION, pid: std::process::id(), uptime_secs: control.started.elapsed().as_secs(), error: None, applied: applied_snapshot() };
                let kind = match &work.command { Command::GetConfig => 1, Command::ExplainRoute(_) => 2, Command::Runtime(_) => 3, Command::Monitor => 4, Command::FakeIpLookup { .. } => 5, Command::RuntimeTelemetry { .. } => 6, _ => 0 };
                let mutation = matches!(&work.command, Command::Reload { .. } | Command::UpdateSubscriptions)
                    || matches!(&work.command, Command::Runtime(command) if !matches!(command, astra_model::runtime::Command::Snapshot));
                let fast = matches!(&work.command, Command::RuntimeTelemetry { .. } | Command::FakeIpLookup { .. } | Command::Monitor | Command::Status | Command::Stop | Command::SetLogLevel(_) | Command::Runtime(astra_model::runtime::Command::Snapshot));
                if jobs.len() >= if fast {64} else {16} { let _ = work.reply.send(failure(kind, "Core is busy; retry shortly".into(), status())); continue; }
                let baseline = status();
                let future: Pin<Box<dyn Future<Output=Response> + Send + '_>> = match work.command {
                    Command::CommitService { boot_id } => { let mut reply = status(); reply.error = if boot_id != astra_platform::lifecycle::boot_id() {Some("Core boot changed; readiness commit rejected".into())} else {control.candidate.commit().err().map(|e|e.to_string())}; Box::pin(async move {Response::Status(reply)}) }
                    Command::Identity => { let mut identity = astra_platform::lifecycle::identity(applied_snapshot())?; identity.capabilities = Some(control.capabilities.clone()); Box::pin(async move { Response::Identity(identity) }) }
                    Command::RuntimeTelemetry { expected_boot_id } => {
                        let sample = control.telemetry.clone();
                        Box::pin(async move {
                            let result = if expected_boot_id != astra_platform::lifecycle::boot_id() {
                                Err("Core boot changed; refresh runtime identity".into())
                            } else if let Some(sample) = sample {
                                sample().await
                            } else {
                                Err("Runtime telemetry is not supported by this core".into())
                            };
                            Response::RuntimeTelemetry(result)
                        })
                    }
                    Command::FakeIpLookup { expected_boot_id, addresses } => {
                        let lookup = control.fake_lookup.clone();
                        let boot_id = astra_platform::lifecycle::boot_id().to_owned();
                        Box::pin(async move {
                            let outcome = if expected_boot_id != boot_id { FakeIpLookupOutcome::CoreChanged }
                            else if addresses.len() > MAX_FAKE_IP_LOOKUP { FakeIpLookupOutcome::InvalidRequest("At most 64 addresses are accepted".into()) }
                            else if let Some(lookup) = lookup {
                                match lookup(addresses.clone()).await {
                                    Ok(mappings) if mappings.len() == addresses.len() && mappings.iter().zip(&addresses).all(|(mapping, address)| mapping.address == *address && mapping.domain.as_ref().is_none_or(|name| !name.is_empty() && name.len() <= 253 && !name.chars().any(char::is_control))) => FakeIpLookupOutcome::Mappings(mappings),
                                    Ok(_) => FakeIpLookupOutcome::Failed("Invalid mapping provider result".into()),
                                    Err(error) => FakeIpLookupOutcome::Failed(error.chars().take(512).collect()),
                                }
                            } else { FakeIpLookupOutcome::Unsupported };
                            Response::FakeIpLookup(FakeIpLookupResponse { version: 1, boot_id, outcome })
                        })
                    }
                    Command::Monitor => {
                        astra_observe::gauge!("runtime.control_jobs", "控制接口正在处理的任务", "count").set(jobs.len() as u64);
                        astra_observe::gauge!("runtime.control_connections", "控制接口连接数", "count").set(connections.len() as u64);
                        astra_observe::gauge!("runtime.control_queue", "控制接口等待队列", "count").set(commands.len() as u64);
                        let value = astra_observe::snapshot(astra_platform::lifecycle::boot_id());
                        Box::pin(async move { Response::Monitor(Ok(value)) })
                    }
                    Command::GetConfig => { let future = snapshot(); Box::pin(async move { Response::Config(future.await) }) }
                    Command::ExplainRoute(request) => { let future = explain(request); Box::pin(async move { Response::Explanation(future.await) }) }
                    Command::Runtime(command) => { let future = runtime(command); Box::pin(async move { Response::Runtime(future.await) }) }
                    Command::Reload { path } => { let future = reload(path); let mut reply = status(); Box::pin(async move { reply.error = future.await.err(); reply.applied = applied_snapshot(); Response::Status(reply) }) }
                    Command::UpdateSubscriptions => { let future = update(); let mut reply = status(); Box::pin(async move { reply.error = future.await.err(); Response::Status(reply) }) }
                    Command::SetLogLevel(level) => { let mut reply = status(); reply.error = astra_platform::logging::set_level(level).err().map(|e| e.to_string()); Box::pin(async move { Response::Status(reply) }) }
                    _ => { let reply = status(); Box::pin(async move { Response::Status(reply) }) }
                };
                jobs.push(Box::pin(async move {
                    progress::scope(work.progress, finish_request(future, work.reply, kind, baseline, mutation, OPERATION_DEADLINE)).await;
                }));
            }
            Some(_) = jobs.next(), if !jobs.is_empty() => {},
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

fn failure(kind: u8, error: String, mut status: Status) -> Response {
    match kind {
        1 => Response::Config(Err(error)),
        2 => Response::Explanation(Err(error)),
        3 => Response::Runtime(Err(error)),
        4 => Response::Monitor(Err(error)),
        6 => Response::RuntimeTelemetry(Err(error)),
        5 => Response::FakeIpLookup(FakeIpLookupResponse {
            version: 1,
            boot_id: astra_platform::lifecycle::boot_id().into(),
            outcome: FakeIpLookupOutcome::Failed(error),
        }),
        _ => {
            status.error = Some(error);
            Response::Status(status)
        }
    }
}
async fn serve(
    mut stream: astra_platform::platform::LocalStream,
    sender: mpsc::Sender<Work>,
    stopped: mpsc::Sender<()>,
) -> io::Result<()> {
    use tokio::io::AsyncReadExt;
    let first: Request = timeout(Duration::from_secs(3), crate::receive(&mut stream))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "management handshake timeout"))??;
    if first.api_version != crate::API_VERSION {
        return Err(io::Error::other("unsupported management version"));
    }
    let session = matches!(first.command, Command::Session);
    let mut next = Some(first);
    if session {
        send(&mut stream, &Event::<Response>::Ready(crate::API_VERSION)).await?;
        next = None;
    }
    loop {
        let request = match next.take() {
            Some(request) => request,
            None => crate::receive::<Request>(&mut stream).await?,
        };
        if request.api_version != crate::API_VERSION {
            return Err(io::Error::other("unsupported management version"));
        }
        if let Command::WatchRuntime { interval_ms } = request.command {
            let mut tick =
                tokio::time::interval(Duration::from_millis(interval_ms.clamp(100, 5000)));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! { _ = tick.tick() => {}, _ = stream.read_u8() => return Ok(()) }
                let (reply, receiver) = oneshot::channel();
                let (progress, _) = tokio::sync::watch::channel(None);
                sender
                    .send(Work {
                        command: Command::Runtime(astra_model::runtime::Command::Snapshot),
                        reply,
                        progress,
                    })
                    .await
                    .map_err(io::Error::other)?;
                let reply = receiver.await.map_err(io::Error::other)?;
                write_response(&mut stream, reply, session).await?;
            }
        }
        let identified = match &request.command {
            Command::StopIdentified { boot_id, force } => {
                if boot_id != astra_platform::lifecycle::boot_id() {
                    return Err(io::Error::other(
                        "core boot identity changed; stop rejected",
                    ));
                }
                Some(*force)
            }
            _ => None,
        };
        let stop = matches!(request.command, Command::Stop) || identified.is_some();
        let (reply, mut response) = oneshot::channel();
        let (progress, mut updates) = tokio::sync::watch::channel(None);
        sender
            .send(Work {
                command: request.command,
                reply,
                progress,
            })
            .await
            .map_err(io::Error::other)?;
        let reply = loop {
            tokio::select! {
                reply = &mut response => break reply.map_err(io::Error::other)?,
                changed = updates.changed(), if session => {
                    if changed.is_ok() {
                        let event = updates.borrow_and_update().clone();
                        if let Some(event) = event { send(&mut stream, &Event::<Response>::Progress(event)).await?; }
                    } else { break response.await.map_err(io::Error::other)?; }
                }
            }
        };
        write_response(&mut stream, reply, session).await?;
        if identified == Some(true) {
            // An authenticated caller requested termination of this exact boot.
            // The independent network guardian handles resources after forced exit.
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(25));
                std::process::exit(1);
            });
        }
        if stop {
            let _ = stopped.send(()).await;
            return Ok(());
        }
        if !session {
            return Ok(());
        }
    }
}
async fn send(
    stream: &mut astra_platform::platform::LocalStream,
    value: &(impl Serialize + Sync),
) -> io::Result<()> {
    timeout(
        Duration::from_secs(3),
        crate::send_management(stream, value),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "slow management subscriber"))?
}
async fn write_response(
    stream: &mut astra_platform::platform::LocalStream,
    response: Response,
    session: bool,
) -> io::Result<()> {
    if session {
        return send(stream, &Event::Reply(response)).await;
    }
    match response {
        Response::Identity(value) => {
            timeout(Duration::from_secs(3), identity_wire::send(stream, &value))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "slow identity subscriber"))?
        }
        Response::FakeIpLookup(value) => send(stream, &value).await,
        Response::Status(value) => send(stream, &value).await,
        Response::Config(value) => send(stream, &value).await,
        Response::Explanation(value) => send(stream, &value).await,
        Response::Runtime(value) => send(stream, &value).await,
        Response::RuntimeTelemetry(value) => send(stream, &value).await,
        Response::Monitor(value) => send(stream, &value).await,
    }
}

#[cfg(test)]
#[path = "../test/control_server_recovery_test.rs"]
mod recovery_tests;
