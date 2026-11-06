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

pub struct Start {
    pub id: u64,
    pub input: OperationInput,
    pub action: Action,
    pub prompt: Option<(Prompt, String)>,
    pub origin: Origin,
}
pub enum BackendCmd {
    Start(Box<Start>),
    Runtime(Option<(String, u32)>),
    Cancel(u64),
    Checkpoint(Box<astra_client::state::Session>),
}

pub enum BackendEvent {
    Ready(Box<(Prepared, astra_client::state::Session)>),
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
    thread: Option<JoinHandle<()>>,
}
impl Backend {
    pub fn start(options: Options) -> anyhow::Result<Self> {
        // The shared Jobs executor enforces the eight-operation limit. The inbox
        // also carries the final checkpoint, which must not be lost on a full queue.
        let (commands, incoming) = async_channel::unbounded();
        let (outgoing, events) = async_channel::bounded(256);
        let thread = std::thread::Builder::new()
            .name("astra-gui-backend".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => tokio::task::LocalSet::new()
                        .block_on(&runtime, run(options, incoming, outgoing)),
                    Err(error) => {
                        let _ = outgoing.send_blocking(BackendEvent::Failed(error.to_string()));
                    }
                }
            })?;
        Ok(Self {
            commands,
            events,
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

async fn run(options: Options, commands: Receiver<BackendCmd>, events: Sender<BackendEvent>) {
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
    let waker = Waker::new(move || wake.notify_one());
    let mut jobs = Jobs::new(&socket, waker.clone());
    let mut list = Feed::start(socket.clone(), Action::WatchList, waker.clone());
    let mut updates = Feed::start(socket.clone(), Action::WatchUpdates, waker.clone());
    let mut runtime: Option<((String, u32), Feed)> = None;
    let mut info_timer = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Err(_) => break,
                Ok(BackendCmd::Checkpoint(session)) => { if let Err(error) = session.save(&session_path) { let _ = events.send(BackendEvent::Failed(format!("{error:#}"))).await; } },
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
                match tokio::time::timeout(Duration::from_secs(3), protocol::request(&socket, astra_management::v2::RequestBody::Query(astra_management::v2::Query::ProgramInfo))).await {
                    Ok(Ok(astra_management::v2::ResponseBody::ProgramInfo(info))) => { let _ = events.send(BackendEvent::ProgramInfo(Box::new(info))).await; }
                    Ok(Err(error)) => { let _ = events.send(BackendEvent::Disconnected { source: "Program info", message: format!("{error:#}") }).await; }
                    _ => {}
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
    // Shared Jobs and Feed owners abort all their tasks on drop, then LocalSet
    // and the runtime drop before the backend thread returns.
}
