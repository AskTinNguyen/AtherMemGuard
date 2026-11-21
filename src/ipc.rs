//! Safe daemon control IPC and live status channel over Win32 named pipes and shared memory.
//! All `unsafe` Win32 calls live in `win.rs`; this module contains zero `unsafe`.

use std::fmt;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::engine::Status;
use crate::{paths, win};

const CONTROL_READ_MAX: usize = 4096;
const CONTROL_IO_TIMEOUT_MS: u32 = 3000;
const CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_PIPE_RETRY: Duration = Duration::from_secs(1);
/// Maximum timed-pause duration accepted over CLI/IPC (1 year).
pub const MAX_PAUSE_MINUTES: u64 = 525_600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ControlCmd {
    Pause { minutes: Option<u64> },
    Resume,
    Reload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ControlResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    NotRunning,
    Invalid(String),
    Failed(String),
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlError::NotRunning => write!(f, "Ather MemGuard is not running."),
            ControlError::Invalid(msg) | ControlError::Failed(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for ControlError {}

pub struct ControlRequest {
    pub cmd: ControlCmd,
    reply: mpsc::SyncSender<Result<(), String>>,
}

impl ControlRequest {
    pub fn respond(self, res: Result<(), String>) {
        let _ = self.reply.send(res);
    }
}

pub struct ControlServer {
    event: win::Handle,
    rx: Option<mpsc::Receiver<ControlRequest>>,
    shutdown: win::PipeShutdown,
    thread: Option<JoinHandle<()>>,
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.shutdown.signal();
        if let Some(rx) = self.rx.take() {
            while let Ok(req) = rx.try_recv() {
                req.respond(Err("daemon shutting down".into()));
            }
            drop(rx);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl ControlServer {
    pub fn start(on_wake: Option<Box<dyn Fn() + Send + Sync>>) -> Option<Self> {
        Self::start_at(&paths::control_pipe(), &paths::control_event(), on_wake)
    }

    fn start_at(
        pipe_name: &str,
        event_name: &str,
        on_wake: Option<Box<dyn Fn() + Send + Sync>>,
    ) -> Option<Self> {
        let event = win::create_event(event_name)?;
        let wake_event = win::create_event(event_name)?;
        let (mut pipe, shutdown) = match win::PipeServer::bind(pipe_name) {
            Ok(pair) => pair,
            Err(e) => {
                crate::log::write(&format!("ipc: failed to bind control pipe: {}", e));
                return None;
            }
        };
        let (tx, rx) = mpsc::channel::<ControlRequest>();
        let thread = std::thread::Builder::new()
            .name("amg-ipc".into())
            .spawn(move || {
                while let Ok(Some(conn)) = pipe.accept() {
                    let resp = match conn.read_line_timeout(CONTROL_READ_MAX, CONTROL_IO_TIMEOUT_MS) {
                        Ok(line) => match serde_json::from_str::<ControlCmd>(line.trim()) {
                            Ok(cmd) => {
                                if let Err(e) = validate_command(&cmd) {
                                    ControlResponse { ok: false, error: Some(e.to_string()) }
                                } else {
                                    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
                                    if tx.send(ControlRequest { cmd, reply: reply_tx }).is_err() {
                                        ControlResponse { ok: false, error: Some("daemon shutting down".into()) }
                                    } else {
                                        win::set_event_handle(&wake_event);
                                        if let Some(wake) = &on_wake {
                                            wake();
                                        }
                                        match reply_rx.recv_timeout(CONTROL_REPLY_TIMEOUT) {
                                            Ok(Ok(())) => ControlResponse { ok: true, error: None },
                                            Ok(Err(e)) => ControlResponse { ok: false, error: Some(e) },
                                            Err(mpsc::RecvTimeoutError::Disconnected) => {
                                                ControlResponse { ok: false, error: Some("daemon shutting down".into()) }
                                            }
                                            Err(mpsc::RecvTimeoutError::Timeout) => ControlResponse {
                                                ok: false,
                                                error: Some("daemon timed out handling command".into()),
                                            },
                                        }
                                    }
                                }
                            }
                            Err(e) => ControlResponse { ok: false, error: Some(format!("invalid control command: {}", e)) },
                        },
                        Err(e) => ControlResponse { ok: false, error: Some(e) },
                    };
                    if let Ok(mut bytes) = serde_json::to_vec(&resp) {
                        bytes.push(b'\n');
                        let _ = conn.write_all_flush(&bytes, CONTROL_IO_TIMEOUT_MS);
                    }
                }
            })
            .ok()?;

        Some(Self { event, rx: Some(rx), shutdown, thread: Some(thread) })
    }

    pub fn event_wait_handle(&self) -> win::WaitHandle {
        self.event.wait_handle()
    }

    pub fn try_recv(&self) -> Option<ControlRequest> {
        self.rx.as_ref()?.try_recv().ok()
    }
}

pub fn validate_command(cmd: &ControlCmd) -> Result<(), ControlError> {
    if let ControlCmd::Pause { minutes: Some(m) } = cmd {
        if *m == 0 {
            return Err(ControlError::Invalid("--minutes must be at least 1.".into()));
        }
        if *m > MAX_PAUSE_MINUTES {
            return Err(ControlError::Invalid(format!(
                "--minutes must be at most {} (1 year).",
                MAX_PAUSE_MINUTES
            )));
        }
    }
    Ok(())
}

pub fn send_command(cmd: ControlCmd) -> Result<(), ControlError> {
    validate_command(&cmd)?;
    if !win::instance_running(paths::mutex()) {
        return Err(ControlError::NotRunning);
    }
    send_command_to(&paths::control_pipe(), cmd, || win::instance_running(paths::mutex()))
}

fn send_command_to(
    pipe_name: &str,
    cmd: ControlCmd,
    mut daemon_alive: impl FnMut() -> bool,
) -> Result<(), ControlError> {
    validate_command(&cmd)?;
    let mut payload = serde_json::to_vec(&cmd).map_err(|e| ControlError::Failed(e.to_string()))?;
    payload.push(b'\n');

    let retry_until = Instant::now() + STARTUP_PIPE_RETRY;
    let raw = loop {
        match win::pipe_call(pipe_name, &payload, 5000) {
            Ok(bytes) => break bytes,
            Err(win::PipeError::NotFound) => {
                if Instant::now() < retry_until && daemon_alive() {
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                return Err(ControlError::NotRunning);
            }
            Err(win::PipeError::Busy) => return Err(ControlError::Failed("daemon control pipe is busy".into())),
            Err(win::PipeError::Other(e)) => return Err(ControlError::Failed(e)),
        }
    };

    let resp: ControlResponse = serde_json::from_slice(&raw)
        .map_err(|e| ControlError::Failed(format!("invalid control reply: {}", e)))?;
    if resp.ok {
        Ok(())
    } else {
        Err(ControlError::Failed(resp.error.unwrap_or_else(|| "daemon rejected command".into())))
    }
}

// ---------------------------------------------------------------- live status channel

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveUeStatus {
    #[serde(default)]
    pub workers_paused: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveGroupStatus {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub suspended: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveStatus {
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub ue: LiveUeStatus,
    #[serde(default)]
    pub groups: Vec<LiveGroupStatus>,
}

impl LiveStatus {
    pub fn from_status(st: &Status) -> Self {
        Self {
            paused: st.paused,
            ue: LiveUeStatus { workers_paused: st.ue.workers_paused },
            groups: st
                .groups
                .iter()
                .map(|g| LiveGroupStatus { name: g.name.clone(), suspended: g.suspended })
                .collect(),
        }
    }
}

#[derive(Default)]
pub struct StatusPublisher(Option<win::SharedMappingWriter>);

impl StatusPublisher {
    pub fn new() -> Self {
        Self(win::SharedMappingWriter::create(&paths::status_mapping()))
    }

    pub fn publish(&mut self, st: &Status) {
        if self.0.is_none() {
            self.0 = win::SharedMappingWriter::create(&paths::status_mapping());
        }
        if let Some(writer) = self.0.as_mut() {
            Self::publish_to_writer(writer, st);
        }
    }

    fn publish_to_writer(writer: &mut win::SharedMappingWriter, st: &Status) {
        let wrote_full = serde_json::to_vec(st)
            .map(|bytes| writer.write(&bytes))
            .unwrap_or(false);
        if !wrote_full {
            let compact = LiveStatus::from_status(st);
            if let Ok(bytes) = serde_json::to_vec(&compact) {
                writer.write(&bytes);
            }
        }
    }
}

pub fn read_live_status() -> Option<LiveStatus> {
    if let Some(bytes) = win::read_shared_mapping(&paths::status_mapping()) {
        if let Ok(live) = serde_json::from_slice::<LiveStatus>(&bytes) {
            return Some(live);
        }
    }
    let bytes = std::fs::read(paths::status_file()).ok()?;
    serde_json::from_slice::<LiveStatus>(&bytes).ok()
}

pub fn apply_live_status(st: &mut Status) {
    let Some(live) = read_live_status() else { return };
    apply_live_overlay(st, &live);
}

fn apply_live_overlay(st: &mut Status, live: &LiveStatus) {
    st.paused = live.paused;
    st.ue.workers_paused = live.ue.workers_paused;
    for g in &mut st.groups {
        if let Some(lg) = live.groups.iter().find(|lg| lg.name == g.name) {
            g.suspended = lg.suspended;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn pause_zero_minutes_rejected_before_daemon_check() {
        let err = validate_command(&ControlCmd::Pause { minutes: Some(0) }).unwrap_err();
        assert!(matches!(err, ControlError::Invalid(_)));
        assert!(!err.to_string().to_ascii_lowercase().contains("not running"));

        let err2 = send_command_to(r"\\.\pipe\AtherMemGuard.DoesNotExist", ControlCmd::Pause { minutes: Some(0) }, || false)
            .unwrap_err();
        assert!(matches!(err2, ControlError::Invalid(_)));
    }

    #[test]
    fn missing_pipe_returns_not_running() {
        let pipe = format!(r"\\.\pipe\AtherMemGuard.IpcMissing.{}.{}", win::current_pid(), win::now_filetime());
        let err = send_command_to(&pipe, ControlCmd::Resume, || false).unwrap_err();
        assert_eq!(err, ControlError::NotRunning);
    }

    #[test]
    fn control_server_roundtrip_handles_back_to_back_commands_and_wake() {
        let tag = format!("{}.{}", win::current_pid(), win::now_filetime());
        let pipe = format!(r"\\.\pipe\AtherMemGuard.IpcTest.{}", tag);
        let event = format!(r"Local\AtherMemGuard.IpcEventTest.{}", tag);

        let woke = Arc::new(AtomicBool::new(false));
        let woke2 = Arc::clone(&woke);
        let server = ControlServer::start_at(
            &pipe,
            &event,
            Some(Box::new(move || {
                woke2.store(true, Ordering::SeqCst);
            })),
        )
        .expect("start ControlServer");

        let ev_h = server.event_wait_handle();
        let client = std::thread::spawn(move || {
            send_command_to(&pipe, ControlCmd::Pause { minutes: Some(5) }, || true).unwrap();
            send_command_to(&pipe, ControlCmd::Reload, || true).unwrap();
            send_command_to(&pipe, ControlCmd::Resume, || true).unwrap();
        });

        let mut received = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while received.len() < 3 && Instant::now() < deadline {
            match win::wait_any(&[ev_h], 200) {
                win::WaitOutcome::Signaled(0) | win::WaitOutcome::Timeout => {
                    while let Some(req) = server.try_recv() {
                        received.push(req.cmd.clone());
                        req.respond(Ok(()));
                    }
                }
                _ => break,
            }
        }
        client.join().unwrap();
        assert!(woke.load(Ordering::SeqCst));
        assert_eq!(
            received,
            vec![
                ControlCmd::Pause { minutes: Some(5) },
                ControlCmd::Reload,
                ControlCmd::Resume,
            ]
        );
    }

    #[test]
    fn live_status_overlay_parses_status_json_and_updates_fields() {
        let sample = br#"{"paused":true,"ue":{"workers_paused":4},"groups":[{"name":"Browsers","suspended":true}]}"#;
        let live: LiveStatus = serde_json::from_slice(sample).unwrap();
        assert!(live.paused);
        assert_eq!(live.ue.workers_paused, 4);
        assert_eq!(live.groups, vec![LiveGroupStatus { name: "Browsers".into(), suspended: true }]);

        let cfg = crate::config::Config::builtin();
        let mut st = crate::engine::analyze(&cfg, &crate::engine::Snapshot {
            sys: win::SysInfo::default(),
            procs: Vec::new(),
            now: 0,
        });
        assert!(!st.paused);
        apply_live_overlay(&mut st, &live);
        assert!(st.paused);
        assert_eq!(st.ue.workers_paused, 4);
        assert!(st.groups.iter().find(|g| g.name == "Browsers").is_some_and(|g| g.suspended));
    }

    #[test]
    fn pause_excessive_minutes_rejected_before_daemon_check() {
        let err = validate_command(&ControlCmd::Pause { minutes: Some(u64::MAX) }).unwrap_err();
        assert!(matches!(err, ControlError::Invalid(_)));
        assert!(!err.to_string().to_ascii_lowercase().contains("not running"));

        let err2 = validate_command(&ControlCmd::Pause { minutes: Some(MAX_PAUSE_MINUTES + 1) }).unwrap_err();
        assert!(matches!(err2, ControlError::Invalid(_)));

        let err3 = send_command_to(
            r"\\.\pipe\AtherMemGuard.DoesNotExist",
            ControlCmd::Pause { minutes: Some(u64::MAX) },
            || false,
        )
        .unwrap_err();
        assert!(matches!(err3, ControlError::Invalid(_)));

        assert!(validate_command(&ControlCmd::Pause { minutes: Some(MAX_PAUSE_MINUTES) }).is_ok());
    }

    #[test]
    fn control_server_drop_with_unread_request_finishes_quickly() {
        let tag = format!("drop.{}.{}", win::current_pid(), win::now_filetime());
        let pipe = format!(r"\\.\pipe\AtherMemGuard.IpcTest.{}", tag);
        let event = format!(r"Local\AtherMemGuard.IpcEventTest.{}", tag);

        let server = ControlServer::start_at(&pipe, &event, None).expect("start ControlServer");
        let ev_h = server.event_wait_handle();
        let pipe_for_client = pipe.clone();
        let client = std::thread::spawn(move || send_command_to(&pipe_for_client, ControlCmd::Reload, || true));

        // Wait until the server thread receives the command and signals the event (request is queued in rx).
        let wait_res = win::wait_any(&[ev_h], 3000);
        assert!(matches!(wait_res, win::WaitOutcome::Signaled(0)));
        std::thread::sleep(Duration::from_millis(20));

        let drop_start = Instant::now();
        drop(server);
        let drop_elapsed = drop_start.elapsed();
        assert!(
            drop_elapsed < Duration::from_millis(500),
            "ControlServer::drop stalled for {:?} with unread request",
            drop_elapsed
        );

        let client_res = client.join().unwrap();
        match client_res {
            Err(ControlError::Failed(msg)) => assert!(msg.contains("shutting down"), "unexpected error: {}", msg),
            other => panic!("expected Failed(shutting down), got {:?}", other),
        }
    }

    #[test]
    fn status_publisher_falls_back_to_compact_live_status_when_oversized() {
        let map_name = format!(r"Local\AtherMemGuard.ShmFallback.{}.{}", win::current_pid(), win::now_filetime());
        let mut writer = win::SharedMappingWriter::create(&map_name).expect("create shared mapping");
        let cfg = crate::config::Config::builtin();
        let mut st = crate::engine::analyze(&cfg, &crate::engine::Snapshot {
            sys: win::SysInfo::default(),
            procs: Vec::new(),
            now: 0,
        });
        StatusPublisher::publish_to_writer(&mut writer, &st);
        let initial_bytes = win::read_shared_mapping(&map_name).expect("initial SHM bytes");
        let initial_live: LiveStatus = serde_json::from_slice(&initial_bytes).unwrap();
        assert!(!initial_live.paused);

        // Exceed 256 KB SHM limit with thousands of UeProcess entries while flipping paused/group state.
        st.paused = true;
        st.ue.workers_paused = 3;
        if let Some(g) = st.groups.first_mut() {
            g.suspended = true;
        }
        for i in 0..2500u32 {
            st.ue.processes.push(crate::engine::UeProcess {
                pid: 10_000 + i,
                name: format!("UnrealEditor-Worker-Process-{:04}", i),
                kind: "editor",
                committed_gb: 1.5,
                ram_gb: 1.2,
                uptime_min: 10,
                created: 1,
            });
        }
        let full_len = serde_json::to_vec(&st).unwrap().len();
        assert!(full_len > 262_128, "expected oversized payload, got {} bytes", full_len);

        StatusPublisher::publish_to_writer(&mut writer, &st);
        let updated_bytes = win::read_shared_mapping(&map_name).expect("updated SHM bytes");
        let updated_live: LiveStatus = serde_json::from_slice(&updated_bytes).unwrap();
        assert!(updated_live.paused, "SHM must reflect updated paused=true via compact fallback");
        assert_eq!(updated_live.ue.workers_paused, 3);
        assert!(updated_live.groups.first().is_some_and(|g| g.suspended));
    }
}
