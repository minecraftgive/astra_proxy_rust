//! The only GUI module that performs manager I/O. Its executor never runs on GPUI.
use astra_client::{
    bootstrap::{self, Options, Prepared},
    operations::{JobResult, OperationInput},
    protocol::{self, Action, Reply},
    state::Prompt,
};
use astra_client::{
    feeds::Feed,
    jobs::{Event as JobEvent, Jobs},
    waker::Waker,
};
use async_channel::{Receiver, Sender};
use std::{thread::JoinHandle, time::Duration};

pub use astra_client::jobs::Origin;
pub(crate) mod elevation;

pub struct Start {
    pub id: u64,
    pub input: OperationInput,
    pub action: Action,
    pub prompt: Option<(Prompt, String)>,
    pub origin: Origin,
}
pub enum BackendCmd {
    CheckpointForQuit {
        session: Box<astra_client::state::Session>,
    },
    Start(Box<Start>),
    Runtime(Option<(String, u32)>),
    Cancel(u64),
    Checkpoint(Box<astra_client::state::Session>),
    CheckpointForRestart {
        id: u64,
        session: Box<astra_client::state::Session>,
    },
    RefreshProgramInfo,
}

pub enum BackendEvent {
    QuitCheckpoint(Result<(), String>),
    Terminate,
    Ready(Box<(Prepared, astra_client::state::Session)>),
    RestartCheckpoint {
        id: u64,
        result: Result<(), String>,
    },
    LocalProgramInfo(Box<astra_management::v2::ProgramInfo>),
    Progress {
        id: u64,
        value: astra_ipc::progress::Progress,
    },
    JobDone {
        id: u64,
        action: Box<Action>,
        result: Box<JobResult>,
        origin: Origin,
    },
    Rejected {
        id: u64,
        message: String,
    },
    Rows(Box<Reply>),
    Runtime {
        name: String,
        pid: u32,
        snapshot: Box<astra_model::runtime::RuntimeView>,
    },
    Updates(Box<protocol::updates::UpdateStatus>),
    ProgramInfo(Box<astra_management::v2::ProgramInfo>),
    Disconnected {
        source: &'static str,
        message: String,
    },
    Failed(String),
}

pub struct Backend {
    pub commands: Sender<BackendCmd>,
    pub events: Receiver<BackendEvent>,
    pub handle: tokio::runtime::Handle,
    pub waker: Waker,
    pub notifications: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}
impl Backend {
    pub fn start(options: Options) -> anyhow::Result<Self> {
        astra_platform::tui_log::use_gui_log();
        // The shared Jobs executor enforces the eight-operation limit. The inbox
        // also carries the final checkpoint, which must not be lost on a full queue.
        let (commands, incoming) = async_channel::unbounded();
        let (outgoing, events) = async_channel::bounded(256);
        let (wake_sender, notifications) = async_channel::bounded(1);
        let waker = Waker::new(move || {
            let _ = wake_sender.try_send(());
        });
        let frontend_waker = waker.clone();
        let (started, starting) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("astra-gui-backend".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => {
                        if started.send(Ok(runtime.handle().clone())).is_ok() {
                            tokio::task::LocalSet::new().block_on(&runtime, async {
                                run(options, incoming, outgoing, frontend_waker).await;
                                elevation::shutdown().await;
                            });
                            // OS-owned consent/install work may outlive its window.
                            // Never let a blocking UAC wait hold normal GUI exit forever.
                            runtime.shutdown_timeout(Duration::from_secs(2));
                        }
                    }
                    Err(error) => {
                        let _ = started.send(Err(error));
                    }
                }
            })?;
        let handle = starting
            .recv()
            .map_err(|_| anyhow::anyhow!("Backend thread stopped before initialization"))??;
        Ok(Self {
            commands,
            events,
            handle,
            waker,
            notifications,
            thread: Some(thread),
        })
    }
    pub fn shutdown(mut self) {
        self.commands.close();
        while self.events.recv_blocking().is_ok() {}
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn run(
    options: Options,
    commands: Receiver<BackendCmd>,
    events: Sender<BackendEvent>,
    frontend_waker: Waker,
) {
    #[cfg(unix)]
    let _signals = {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("Install SIGTERM handler");
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("Install SIGINT handler");
        let events = events.clone();
        tokio::spawn(async move {
            tokio::select! { _ = terminate.recv() => {}, _ = interrupt.recv() => {} }
            let _ = events.send(BackendEvent::Terminate).await;
        })
    };
    let prepared = match bootstrap::prepare(&options).await {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = events
                .send(BackendEvent::Failed(format!("{error:#}")))
                .await;
            return;
        }
    };
    let session_path = match prepared.launch.session_path(&prepared.context.home) {
        Ok(path) => path,
        Err(error) => {
            let _ = events
                .send(BackendEvent::Failed(format!("{error:#}")))
                .await;
            return;
        }
    };
    let session = match astra_client::state::Session::load(&session_path) {
        Ok(session) => session,
        Err(error) => {
            let _ = events
                .send(BackendEvent::Failed(format!("{error:#}")))
                .await;
            return;
        }
    };
    let socket = prepared.socket.clone();
    if events
        .send(BackendEvent::Ready(Box::new((prepared, session))))
        .await
        .is_err()
    {
        return;
    }
    let notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let wake = notify.clone();
    let waker = Waker::new(move || {
        wake.notify_one();
        frontend_waker.wake();
    });
    let mut jobs = Jobs::new(&socket, waker.clone());
    let mut list = Feed::start(socket.clone(), Action::WatchList, waker.clone());
    let mut updates = Feed::start(socket.clone(), Action::WatchUpdates, waker.clone());
    let mut runtime: Option<((String, u32), Feed)> = None;
    let mut info_timer = tokio::time::interval(Duration::from_secs(30));
    let mut info_task: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Err(_) => break,
                Ok(BackendCmd::CheckpointForQuit { session }) => {
                    let result = session.save(&session_path).map_err(|error| format!("{error:#}"));
                    let _ = events.send(BackendEvent::QuitCheckpoint(result)).await;
                },
                Ok(BackendCmd::Checkpoint(session)) => { if let Err(error) = session.save(&session_path) { let _ = events.send(BackendEvent::Failed(format!("{error:#}"))).await; } },
                Ok(BackendCmd::CheckpointForRestart { id, session }) => {
                    let _ = events.send(checkpoint_for_restart(&session_path, id, &session)).await;
                },
                Ok(BackendCmd::RefreshProgramInfo) => info_timer.reset_immediately(),
                Ok(BackendCmd::Cancel(id)) => jobs.cancel(id),
                Ok(BackendCmd::Runtime(key)) => {
                    if key.as_ref() != runtime.as_ref().map(|(key, _)| key) {
                        runtime = key.map(|(name, pid)| {
                            let feed = Feed::start(socket.clone(), Action::WatchRuntime { name: name.clone(), expected_pid: pid, interval_ms: 500 }, waker.clone());
                            ((name, pid), feed)
                        });
                    }
                }
                Ok(BackendCmd::Start(request)) => {
                    let Start { id, input, action, prompt, origin } = *request;
                    // UI-generated IDs remain monotonic, including rejected operations.
                    jobs.next = id - 1;
                    if let Err(error) = jobs.start(input, action, prompt, origin) {
                        let _ = events.send(BackendEvent::Rejected { id, message: error.to_string() }).await;
                    }
                }
            },
            _ = notify.notified() => {},
            _ = info_timer.tick() => {
                if info_task.as_ref().is_none_or(|task| task.is_finished()) {
                    let socket = socket.clone();
                    let events = events.clone();
                    info_task = Some(tokio::spawn(async move { refresh_information(&socket, &events).await; }));
                }
            }
        }
        if events.is_closed() {
            break;
        }
        for event in jobs.poll().await {
            let event = match event {
                JobEvent::Progress { id, value } => BackendEvent::Progress { id, value },
                JobEvent::Done {
                    id,
                    action,
                    origin,
                    result: Ok(result),
                } => BackendEvent::JobDone {
                    id,
                    action: Box::new(action),
                    origin,
                    result,
                },
                JobEvent::Done {
                    id,
                    result: Err(message),
                    ..
                } => BackendEvent::Rejected { id, message },
            };
            if events.send(event).await.is_err() {
                return;
            }
        }
        if let Some(reply) = list.take() {
            let event = match reply {
                Ok(reply) => BackendEvent::Rows(Box::new(reply)),
                Err(message) => BackendEvent::Disconnected {
                    source: "Manager",
                    message,
                },
            };
            if events.send(event).await.is_err() {
                return;
            }
        }
        if let Some(reply) = updates.take() {
            let event = match reply.and_then(|reply| {
                serde_json::from_str(&reply.text).map_err(|error| error.to_string())
            }) {
                Ok(status) => BackendEvent::Updates(Box::new(status)),
                Err(message) => BackendEvent::Disconnected {
                    source: "Updates",
                    message,
                },
            };
            if events.send(event).await.is_err() {
                return;
            }
        }
        if let Some(((name, pid), feed)) = &mut runtime {
            if let Some(reply) = feed.take() {
                let event = match reply.and_then(|reply| {
                    serde_json::from_str(&reply.text).map_err(|error| error.to_string())
                }) {
                    Ok(snapshot) => BackendEvent::Runtime {
                        name: name.clone(),
                        pid: *pid,
                        snapshot: Box::new(snapshot),
                    },
                    Err(message) => BackendEvent::Disconnected {
                        source: "Runtime",
                        message,
                    },
                };
                if events.send(event).await.is_err() {
                    return;
                }
            }
        }
    }
    if let Some(task) = info_task {
        task.abort();
    }
    // Shared Jobs and Feed owners abort all their tasks on drop, then LocalSet
    // and the runtime drop before the backend thread returns.
}

async fn refresh_information(socket: &std::path::Path, events: &Sender<BackendEvent>) {
    let local = tokio::task::spawn_blocking(|| {
        astra_platform::program_info::ProgramInfo::collect(astra_platform::build_info!(), vec![])
    });
    let remote = tokio::time::timeout(
        Duration::from_secs(3),
        protocol::request(
            socket,
            astra_management::v2::RequestBody::Query(astra_management::v2::Query::ProgramInfo),
        ),
    );
    let (local, remote) = tokio::join!(local, remote);
    match local {
        Ok(info) => {
            let _ = events
                .send(BackendEvent::LocalProgramInfo(Box::new(info)))
                .await;
        }
        Err(error) => {
            let _ = events
                .send(BackendEvent::Disconnected {
                    source: "Local program info",
                    message: error.to_string(),
                })
                .await;
        }
    }
    let result = match remote {
        Ok(Ok(astra_management::v2::ResponseBody::ProgramInfo(info))) => Ok(info),
        Ok(Ok(_)) => Err(astra_client::i18n::t("Unexpected program information response").into()),
        Ok(Err(error)) => Err(format!("{error:#}")),
        Err(error) => Err(error.to_string()),
    };
    match result {
        Ok(info) => {
            let _ = events.send(BackendEvent::ProgramInfo(Box::new(info))).await;
        }
        Err(message) => {
            let _ = events
                .send(BackendEvent::Disconnected {
                    source: "Program info",
                    message,
                })
                .await;
        }
    }
}

fn checkpoint_for_restart(
    path: &std::path::Path,
    id: u64,
    session: &astra_client::state::Session,
) -> BackendEvent {
    let result = session.save(path).map_err(|error| format!("{error:#}"));
    BackendEvent::RestartCheckpoint { id, result }
}
#[cfg(test)]
#[path = "test/backend_test.rs"]
mod tests;
