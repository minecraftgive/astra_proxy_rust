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
    R: FnMut(crate::runtime::Command) -> RF,
    RF: Future<Output = Result<crate::runtime::Snapshot, String>> + Send,
{
    let (sender, mut commands) = mpsc::channel::<Work>(64);
    let (stopped, mut stop) = mpsc::channel::<()>(1);
    let permits = Arc::new(Semaphore::new(64));
    let mut connections = JoinSet::new();
    let mut jobs: FuturesUnordered<Pin<Box<dyn Future<Output = ()> + Send + '_>>> =
        FuturesUnordered::new();
    let signal = crate::shutdown_signal();
    tokio::pin!(signal);
    let mut hup = crate::platform::ReloadSignal::new()?;
    loop {
        let accept = async {
            match &control.listener {
                Some(listener) => listener.accept().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            result = &mut signal => { result?; break; }
            _ = stop.recv() => break,
            _ = hup.recv() => {
                if jobs.len() < 16 {
                    let future = reload(None);
                    jobs.push(Box::pin(async move { if let Ok(Err(error)) = timeout(Duration::from_secs(60), future).await { eprintln!("Configuration reload rejected: {error}"); } }));
                }
            }
            result = accept => {
                let (stream, _) = result?;
                if let Ok(permit) = permits.clone().try_acquire_owned() {
                    let (sender, stopped) = (sender.clone(), stopped.clone());
                    connections.spawn(async move { let _permit = permit; let _ = serve(stream, sender, stopped).await; });
                }
            }
            Some(work) = commands.recv() => {
                if work.reply.is_closed() { continue; }
                let status = || Status { api_version: crate::API_VERSION, pid: std::process::id(), uptime_secs: control.started.elapsed().as_secs(), error: None, applied: APPLIED.read().unwrap().clone() };
                let kind = match &work.command { Command::GetConfig => 1, Command::ExplainRoute(_) => 2, Command::Runtime(_) => 3, _ => 0 };
                let fast = matches!(&work.command, Command::Status | Command::Stop | Command::SetLogLevel(_) | Command::Runtime(crate::runtime::Command::Snapshot));
                if jobs.len() >= if fast {64} else {16} { let _ = work.reply.send(failure(kind, "Core is busy; retry shortly".into(), status())); continue; }
                let baseline = status();
                let future: Pin<Box<dyn Future<Output=Response> + Send + '_>> = match work.command {
                    Command::GetConfig => { let future = snapshot(); Box::pin(async move { Response::Config(future.await) }) }
                    Command::ExplainRoute(request) => { let future = explain(request); Box::pin(async move { Response::Explanation(future.await) }) }
                    Command::Runtime(command) => { let future = runtime(command); Box::pin(async move { Response::Runtime(future.await) }) }
                    Command::Reload { path } => { let future = reload(path); let mut reply = status(); Box::pin(async move { reply.error = future.await.err(); reply.applied = APPLIED.read().unwrap().clone(); Response::Status(reply) }) }
                    Command::UpdateSubscriptions => { let future = update(); let mut reply = status(); Box::pin(async move { reply.error = future.await.err(); Response::Status(reply) }) }
                    Command::SetLogLevel(level) => { let mut reply = status(); reply.error = crate::logging::set_level(level).err().map(|e| e.to_string()); Box::pin(async move { Response::Status(reply) }) }
                    _ => { let reply = status(); Box::pin(async move { Response::Status(reply) }) }
                };
                jobs.push(Box::pin(async move {
                    let reply = progress::scope(work.progress, timeout(Duration::from_secs(60), future)).await
                        .unwrap_or_else(|_| failure(kind, "Core operation timed out; inspect current state before retrying".into(), baseline));
                    let _ = work.reply.send(reply);
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
        _ => {
            status.error = Some(error);
            Response::Status(status)
        }
    }
}
async fn serve(
    mut stream: crate::platform::LocalStream,
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
                        command: Command::Runtime(crate::runtime::Command::Snapshot),
                        reply,
                        progress,
                    })
                    .await
                    .map_err(io::Error::other)?;
                let reply = receiver.await.map_err(io::Error::other)?;
                write_response(&mut stream, reply, session).await?;
            }
        }
        let stop = matches!(request.command, Command::Stop);
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
    stream: &mut crate::platform::LocalStream,
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
    stream: &mut crate::platform::LocalStream,
    response: Response,
    session: bool,
) -> io::Result<()> {
    if session {
        return send(stream, &Event::Reply(response)).await;
    }
    match response {
        Response::Status(value) => send(stream, &value).await,
        Response::Config(value) => send(stream, &value).await,
        Response::Explanation(value) => send(stream, &value).await,
        Response::Runtime(value) => send(stream, &value).await,
    }
}
