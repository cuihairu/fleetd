//! Machine: the Worker runtime shape (FLEETD-SCHED-MACHINE-001).
//!
//! The whole execution chain abstracts onto Go's CSP model — a machine is a
//! processor, a task is a message, the mailbox (an mpsc channel) is the only
//! communication surface. Dispatch = send, receipt = receive.
//!
//! Go concept mapping (user directive 2026-11-28):
//!   goroutine            -> Machine (one resident logical process per
//!                           machine, N executor slots = max_concurrent)
//!   channel              -> per-machine mailbox: one mpsc channel, drained
//!                           by slot workers bounded by a semaphore
//!   message              -> Job
//!   select               -> Scheduler::run select loop (mailbox / events /
//!                           ticker)
//!   context cancellation -> cancel(job_id): a control message on a dedicated
//!                           channel so cancellation never queues behind jobs
//!   sync.WaitGroup       -> WaitGroup: batch completion counting (split
//!                           batches like boundless A/B converge here)
//!
//! Evolution, not rewrite (worker-design.md): the `Agent` face in agent.rs
//! stays untouched and is wrapped by `Executor` impls (TmuxExecutor pastes
//! into seats, HeadlessExecutor spawns unattended `claude -p` processes).
//! Legacy `hosts` config keeps working via the derived-worker mapping.

use crate::agent::{self, agent_for, AgentKind, Delivery};
use crate::config::FleetConfig;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::process::Command;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Notify, Semaphore};

// ---------------------------------------------------------------------------
// Messages and types
// ---------------------------------------------------------------------------

/// Capability declaration: what a machine can run (worker-design.md Worker).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caps {
    /// Executor kinds: claude / codex / shell / ahk-ui / ...
    pub executors: Vec<String>,
    /// Free-form labels used by constraint routing ("desktop", "gpu", ...).
    pub tags: Vec<String>,
    /// OS family: linux | windows | macos.
    pub os: String,
}

/// Route constraint attached to a Job (EnvSpec). A machine qualifies when it
/// declares every requirement; empty Requires matches any machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Requires {
    /// Executor kind the job needs (None = any).
    pub executor: Option<String>,
    /// Tags the machine must carry (all of them).
    pub tags: Vec<String>,
    /// OS family the job must run on (None = any).
    pub os: Option<String>,
}

impl Caps {
    /// Constraint filter: does this machine satisfy the job's Requires?
    pub fn satisfies(&self, req: &Requires) -> bool {
        if let Some(ex) = &req.executor {
            if !self.executors.iter().any(|e| e == ex) {
                return false;
            }
        }
        if !req.tags.iter().all(|t| self.tags.contains(t)) {
            return false;
        }
        if let Some(os) = &req.os {
            if &self.os != os {
                return false;
            }
        }
        true
    }
}

/// A task message. Dispatching means sending one of these into a mailbox.
#[derive(Debug, Clone, Default)]
pub struct Job {
    /// Caller-assigned id (dispatch row id or timestamp form).
    pub id: String,
    /// Landing site: the tmux seat / workspace name the job runs in.
    pub seat: String,
    /// Task tag for the ledger (tasks/<tag>.md convention).
    pub tag: String,
    /// Task text pasted or launched with.
    pub text: String,
    /// Task file path on this host (headless delivery copies it); empty =
    /// text is the payload and gets materialized into scratch.
    pub task_file: String,
    /// Routing constraints (caps filter).
    pub requires: Requires,
}

/// Receipt of an accepted send: which machine owns the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobHandle {
    pub job_id: String,
    pub machine_id: String,
}

/// Job lifecycle as seen by the machine plane. `Delivered` means the seat
/// accepted the task; the completion verdict itself flows through the Stop
/// hook into the ledger, where the GATE-001 review gate adjudicates
/// (executor reports Completed -> review). The machine plane never judges
/// completion — it only carries state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    /// Accepted into the mailbox, not picked up yet.
    Queued,
    /// A slot consumer is delivering it.
    Running,
    /// Seat accepted the task (esc-to-interrupt seen).
    Delivered,
    /// Delivery failed (session missing / no ack / no executor).
    Failed(String),
    /// Cancelled via the control channel; the seat was stopped.
    Cancelled,
    /// Unknown job on this machine.
    Unknown,
}

impl JobState {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Delivered => "delivered",
            JobState::Failed(_) => "failed",
            JobState::Cancelled => "cancelled",
            JobState::Unknown => "unknown",
        }
    }

    /// Terminal states never transition again (and own the WaitGroup count).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobState::Delivered | JobState::Failed(_) | JobState::Cancelled
        )
    }
}

/// Load snapshot: running / queued / total slots.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Load {
    pub running: usize,
    pub queued: usize,
    pub slots: usize,
}

impl Load {
    /// Ratio used for least-loaded-first routing.
    pub fn ratio(&self) -> f32 {
        if self.slots == 0 {
            f32::MAX
        } else {
            (self.running + self.queued) as f32 / self.slots as f32
        }
    }
}

/// One row of `fleetd machines`: caps + load + mailbox depth.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct MachineStat {
    pub id: String,
    /// local | ssh:<alias> | agent
    pub channel: String,
    pub os: String,
    pub executors: Vec<String>,
    pub tags: Vec<String>,
    /// Jobs currently delivering.
    pub running: usize,
    /// Jobs waiting in the mailbox.
    pub queued: usize,
    /// Total executor slots (= max_concurrent).
    pub slots: usize,
    /// Live process probe on the host (pgrep count), best effort.
    pub procs: usize,
}

/// Backpressure signal: the mailbox is full, try another machine.
/// Route() downcasts this to distinguish "reroute" from "hard error".
#[derive(Debug)]
pub struct MailboxFull {
    pub machine: String,
}

impl fmt::Display for MailboxFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mailbox full on machine {}", self.machine)
    }
}

impl std::error::Error for MailboxFull {}

/// sync.WaitGroup mapping: batch completion counting. add() on submit,
/// done() on every terminal transition, wait() until the batch converges.
#[derive(Debug, Default)]
pub struct WaitGroup {
    count: Mutex<usize>,
    notify: Notify,
}

impl WaitGroup {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&self, n: usize) {
        *self.count.lock().unwrap() += n;
    }

    pub fn done(&self) {
        let mut c = self.count.lock().unwrap();
        *c = c.saturating_sub(1);
        let reached_zero = *c == 0;
        drop(c);
        if reached_zero {
            self.notify.notify_waiters();
        }
    }

    pub fn count(&self) -> usize {
        *self.count.lock().unwrap()
    }

    /// Await batch convergence. Re-checks the count after registering for
    /// notification, so a done() racing the register cannot hang us.
    pub async fn wait(&self) {
        loop {
            if self.count() == 0 {
                return;
            }
            let notified = self.notify.notified();
            if self.count() == 0 {
                return;
            }
            notified.await;
        }
    }
}

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

/// A machine: one resident logical process (Go processor analogy) with a
/// mailbox. Submit = send, status = receipt query, cancel = context
/// cancellation, load = capacity readback.
#[async_trait]
pub trait Machine: Send + Sync {
    /// Worker name (worker id).
    fn id(&self) -> &str;

    /// Capability declaration for constraint routing.
    fn caps(&self) -> &Caps;

    /// Transport face: "local" | "ssh:<alias>" | "agent".
    fn channel(&self) -> &str {
        "local"
    }

    /// Send a job into the mailbox. Full mailbox = MailboxFull (backpressure;
    /// the router reroutes to the next machine).
    async fn submit(&self, job: Job) -> Result<JobHandle>;

    /// Receipt query: job lifecycle state on this machine.
    async fn status(&self, job_id: &str) -> JobState;

    /// Cancel a job and stop its seat (context cancellation).
    async fn cancel(&self, job_id: &str) -> Result<()>;

    /// Capacity readback: running / queued / slots.
    fn load(&self) -> Load;

    /// Wait for this machine's in-flight batch to converge (WaitGroup).
    /// Default: nothing to wait on.
    async fn join(&self) {}
}

// ---------------------------------------------------------------------------
// Executors: the delivery half (Agent face folded into Executor impls)
// ---------------------------------------------------------------------------

/// Delivery half of a machine. The `Agent` face (agent.rs) keeps its plugin
/// role; Executor impls wrap it (worker-design.md: Agent -> Executor).
#[async_trait]
pub trait Executor: Send + Sync {
    /// Executor kind matching Caps.executors / Requires.executor.
    fn kind(&self) -> String;

    /// Deliver the task into its seat. Returns an ack summary.
    async fn deliver(&self, job: &Job) -> std::result::Result<String, String>;

    /// Hard stop for cancel: seat stop layers 1+2 (kill-session, pkill
    /// workdir). Layer 3 (the ledger gate) is store-plane — callers compose
    /// it there so the machine plane stays ledger-free.
    async fn stop(&self, seat: &str) -> std::result::Result<(), String>;
}

/// Paste executor: drives an interactive seat through the agent face
/// (tmux send-keys, local or ssh). This is the tmux_run/agent surface
/// wrapped, exactly as the ticket prescribes.
pub struct TmuxExecutor {
    host: String,
    agent_kind: AgentKind,
}

impl TmuxExecutor {
    pub fn new(host: &str) -> Self {
        Self {
            host: host.to_string(),
            agent_kind: AgentKind::Claude,
        }
    }

    pub fn with_agent(host: &str, agent_spec: &str) -> Self {
        Self {
            host: host.to_string(),
            agent_kind: agent_for(agent_spec),
        }
    }
}

#[async_trait]
impl Executor for TmuxExecutor {
    fn kind(&self) -> String {
        self.agent_kind.as_str().to_string()
    }

    async fn deliver(&self, job: &Job) -> std::result::Result<String, String> {
        let host = self.host.clone();
        let seat = job.seat.clone();
        let text = job.text.clone();
        let kind = self.agent_kind;
        // agent dispatch is blocking IO (send-keys + 3s ack wait) — keep it
        // off the async worker threads.
        tokio::task::spawn_blocking(move || {
            match agent::registry(kind).dispatch(&host, &seat, &text) {
                Delivery::Delivered => Ok(format!("delivered to {seat}@{host}")),
                Delivery::NoSession => Err(format!("{seat} session missing on {host}")),
                Delivery::NotDelivered => Err(format!("{seat} did not acknowledge on {host}")),
            }
        })
        .await
        .map_err(|e| format!("deliver join: {e}"))?
    }

    async fn stop(&self, seat: &str) -> std::result::Result<(), String> {
        let host = self.host.clone();
        let seat = seat.to_string();
        tokio::task::spawn_blocking(move || {
            // Layer 1: the session itself.
            let _ = agent::tmux_run(&host, &["kill-session", "-t", &seat]);
            // Layer 2: processes living in the seat workdir.
            let pkill = if host == "local" {
                Command::new("pkill")
                    .args(["-f", &format!("workspaces/{seat}")])
                    .output()
            } else {
                Command::new("ssh")
                    .args(["-o", "ConnectTimeout=8", "-o", "BatchMode=yes", &host])
                    .arg(format!("pkill -f workspaces/{seat}"))
                    .output()
            };
            let _ = pkill;
            Ok(())
        })
        .await
        .map_err(|e| format!("stop join: {e}"))?
    }
}

/// Headless executor: the unattended dispatch face (headless.rs direction —
/// one `claude -p` process per task, no screen scraping). Delivery mirrors
/// the dispatch CLI byte-for-byte so routing through the mailbox changes
/// nothing observable.
#[derive(Debug, Clone)]
pub struct HeadlessExecutor {
    pub host: String,
    pub os: String,
    pub workdir: String,
    pub scratch: String,
}

impl HeadlessExecutor {
    /// Effective workdir (host default when unset).
    fn workdir(&self) -> String {
        if !self.workdir.is_empty() {
            return self.workdir.clone();
        }
        if self.os == "windows" {
            r"C:\Users\cui".to_string()
        } else {
            "~".to_string()
        }
    }

    fn scratch(&self) -> String {
        if !self.scratch.is_empty() {
            return self.scratch.clone();
        }
        if self.os == "windows" {
            r"C:\Users\cui\fleetd".to_string()
        } else {
            "/tmp/fleetd".to_string()
        }
    }
}

#[async_trait]
impl Executor for HeadlessExecutor {
    fn kind(&self) -> String {
        "claude".to_string()
    }

    async fn deliver(&self, job: &Job) -> std::result::Result<String, String> {
        let ex = self.clone();
        let job = job.clone();
        tokio::task::spawn_blocking(move || ex.deliver_blocking(&job))
            .await
            .map_err(|e| format!("deliver join: {e}"))?
    }

    async fn stop(&self, seat: &str) -> std::result::Result<(), String> {
        let host = self.host.clone();
        let seat = seat.to_string();
        // Layer 2 only: a headless run has no tmux session to kill; the
        // claude -p process in the workspace is the thing to stop.
        tokio::task::spawn_blocking(move || {
            let pkill = if host == "local" {
                Command::new("pkill")
                    .args(["-f", &format!("workspaces/{seat}")])
                    .output()
            } else {
                Command::new("ssh")
                    .args(["-o", "ConnectTimeout=8", "-o", "BatchMode=yes", &host])
                    .arg(format!("pkill -f workspaces/{seat}"))
                    .output()
            };
            let _ = pkill;
            Ok(())
        })
        .await
        .map_err(|e| format!("stop join: {e}"))?
    }
}

impl HeadlessExecutor {
    /// The launch block moved verbatim out of the dispatch CLI: copy the
    /// task file, then nohup claude -p in the workdir. Local / remote
    /// (ssh+scp) / windows variants all preserved.
    fn deliver_blocking(&self, job: &Job) -> std::result::Result<String, String> {
        let workdir = self.workdir();
        let scratch = self.scratch();

        // Task file on this host; materialize text when the job carries none.
        let task_file = if job.task_file.is_empty() {
            std::fs::create_dir_all(&scratch)
                .map_err(|e| format!("mkdir scratch {scratch}: {e}"))?;
            let p = format!("{}/fleetd-task-{}.md", scratch, job.id);
            std::fs::write(&p, &job.text).map_err(|e| format!("write {p}: {e}"))?;
            p
        } else {
            job.task_file.clone()
        };

        if self.host == "local" {
            let _ = std::fs::create_dir_all(&scratch);
            let _ = std::fs::create_dir_all(&workdir);
            let cmd = format!(
                "cd '{}' && nohup claude -p --dangerously-skip-permissions --permission-mode bypassPermissions \"$(cat '{}')\" > '{}/fleetd-{}.log 2>&1 &",
                workdir,
                task_file,
                scratch,
                job.seat
            );
            let ok = Command::new("bash")
                .arg("-c")
                .arg(&cmd)
                .status()
                .map_err(|e| format!("local launch: {e}"))?
                .success();
            if !ok {
                return Err("local launch failed".into());
            }
            return Ok(format!("launched headless on local (workdir {workdir})"));
        }

        // Ensure scratch + workdir exist before scp (prevents "dest open").
        let mkdir_cmd = if self.os == "windows" {
            format!(
                "cmd /c \"if not exist {} mkdir {} & if not exist {} mkdir {}\"",
                scratch, scratch, workdir, workdir
            )
        } else {
            format!("mkdir -p '{}' '{}'", scratch, workdir)
        };
        let _ = Command::new("ssh")
            .args(["-o", "ConnectTimeout=8", &self.host])
            .arg(&mkdir_cmd)
            .status();

        let remote_path = format!("{}/fleetd-{}.md", scratch, job.seat);
        let scp_ok = Command::new("scp")
            .args(["-o", "ConnectTimeout=8", &task_file])
            .arg(format!("{}:{}", self.host, remote_path))
            .status()
            .map_err(|e| format!("scp: {e}"))?
            .success();
        if !scp_ok {
            return Err(format!("scp to {} failed", self.host));
        }
        let remote_cmd = if self.os == "windows" {
            format!(
                "cmd /c \"cd /d {} && claude -p --dangerously-skip-permissions --permission-mode bypassPermissions \\\"$(type {})\\\" > {}\\fleetd-{}.log 2>&1\"",
                workdir, remote_path, scratch, job.seat
            )
        } else {
            format!(
                "cd '{}' && nohup claude -p --dangerously-skip-permissions --permission-mode bypassPermissions \"$(cat '{}')\" > '{}/fleetd-{}.log' 2>&1 &",
                workdir, remote_path, scratch, job.seat
            )
        };
        let ok = Command::new("ssh")
            .args(["-o", "ConnectTimeout=8", &self.host])
            .arg(&remote_cmd)
            .status()
            .map_err(|e| format!("remote launch: {e}"))?
            .success();
        if !ok {
            return Err(format!("remote launch on {} failed", self.host));
        }
        Ok(format!(
            "launched headless on {} (workdir {workdir})",
            self.host
        ))
    }
}

// ---------------------------------------------------------------------------
// MailboxMachine: the resident process (one mailbox, N slot consumers)
// ---------------------------------------------------------------------------

struct MailboxInner {
    /// THE mailbox: one mpsc channel, cap = slots. Full = backpressure.
    tx: mpsc::Sender<Job>,
    /// Control channel: cancel messages never queue behind jobs (context
    /// cancellation semantics — Go's ctx.Done() is not a work item either).
    cancel_tx: mpsc::UnboundedSender<String>,
    states: Mutex<HashMap<String, JobState>>,
    /// id -> job, so cancel can find the seat to stop.
    jobs: Mutex<HashMap<String, Job>>,
    /// The N executor slots (one permit per slot = max_concurrent).
    slots: Arc<Semaphore>,
    /// Total slot count (semaphore permits get parked by the drainer, so
    /// the count itself is stored, not derived from available_permits).
    slot_count: usize,
    wg: WaitGroup,
    executors: Vec<Arc<dyn Executor>>,
}

/// A tmux-backed machine: local seat host or an ssh remote. One resident
/// drainer task pulls jobs off the mailbox; each job runs on its own
/// consumer task bounded by the slot semaphore (the N goroutines of the
/// worker pool), and one resident canceler drains the control channel.
pub struct MailboxMachine {
    id: String,
    caps: Caps,
    inner: Arc<MailboxInner>,
    channel: String,
}

impl MailboxMachine {
    /// Start the resident tasks. Must be called inside a tokio runtime.
    /// Mailbox capacity = slots: a machine admits at most `slots` running
    /// jobs and `slots` queued ones; everything beyond that backpressures.
    pub fn start(id: &str, caps: Caps, slots: usize, executors: Vec<Arc<dyn Executor>>) -> Self {
        let slots = slots.max(1);
        let (tx, mut rx) = mpsc::channel::<Job>(slots);
        let (cancel_tx, mut cancel_rx) = mpsc::unbounded_channel::<String>();
        let inner = Arc::new(MailboxInner {
            tx,
            cancel_tx,
            states: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(slots)),
            slot_count: slots,
            wg: WaitGroup::new(),
            executors,
        });

        // Canceler: the only consumer of the control channel. Cancellation
        // is a control message, not a work item — it must never wait behind
        // queued jobs (Go: ctx cancellation interrupts, it does not queue).
        {
            let inner = Arc::clone(&inner);
            tokio::spawn(async move {
                while let Some(job_id) = cancel_rx.recv().await {
                    cancel_job(&inner, &job_id).await;
                }
            });
        }

        // Mailbox drainer: acquire a slot permit BEFORE pulling the next
        // job, so an all-slots-busy machine leaves jobs in the channel and
        // submit() observes real backpressure instead of silently growing
        // an invisible wait queue.
        {
            let inner = Arc::clone(&inner);
            tokio::spawn(async move {
                loop {
                    let permit = match Arc::clone(&inner.slots).acquire_owned().await {
                        Ok(p) => p,
                        Err(_) => break, // semaphore closed: shutting down
                    };
                    match rx.recv().await {
                        Some(job) => {
                            let inner = Arc::clone(&inner);
                            tokio::spawn(async move {
                                let _permit = permit; // held for the job's life
                                run_job(&inner, job).await;
                            });
                        }
                        None => break, // mailbox closed: machine shutting down
                    }
                }
            });
        }

        let channel = String::new();
        Self {
            id: id.to_string(),
            caps,
            inner,
            channel,
        }
    }

    /// Local machine: seats live in this host's tmux server.
    pub fn local(id: &str, caps: Caps, slots: usize) -> Arc<Self> {
        Arc::new(
            Self::start(
                id,
                caps,
                slots,
                vec![Arc::new(TmuxExecutor::new("local")) as Arc<dyn Executor>],
            )
            .with_channel("local"),
        )
    }

    /// Remote machine: seats live in the host's tmux server, reached via ssh.
    pub fn ssh(alias: &str, caps: Caps, slots: usize) -> Arc<Self> {
        Arc::new(
            Self::start(
                alias,
                caps,
                slots,
                vec![Arc::new(TmuxExecutor::new(alias)) as Arc<dyn Executor>],
            )
            .with_channel(&format!("ssh:{alias}")),
        )
    }

    /// Machine driven by caller-supplied executors (config / custom faces).
    pub fn with_executors(
        id: &str,
        caps: Caps,
        slots: usize,
        executors: Vec<Arc<dyn Executor>>,
        channel: String,
    ) -> Arc<Self> {
        Arc::new(Self::start(id, caps, slots, executors).with_channel(&channel))
    }

    fn with_channel(mut self, channel: &str) -> Self {
        self.channel = channel.to_string();
        self
    }
}

#[async_trait]
impl Machine for MailboxMachine {
    fn id(&self) -> &str {
        &self.id
    }

    fn caps(&self) -> &Caps {
        &self.caps
    }

    fn channel(&self) -> &str {
        &self.channel
    }

    async fn submit(&self, job: Job) -> Result<JobHandle> {
        let job_id = job.id.clone();
        // Bookkeeping lands BEFORE the send: on a multi-thread runtime the
        // drainer may pick the job up the instant it enters the channel,
        // and must never see missing state. Rejected sends roll back below.
        self.inner
            .states
            .lock()
            .unwrap()
            .insert(job_id.clone(), JobState::Queued);
        self.inner
            .jobs
            .lock()
            .unwrap()
            .insert(job_id.clone(), job.clone());
        match self.inner.tx.try_send(job) {
            Ok(()) => {
                self.inner.wg.add(1);
                Ok(JobHandle {
                    job_id,
                    machine_id: self.id.clone(),
                })
            }
            Err(_) => {
                self.inner.states.lock().unwrap().remove(&job_id);
                self.inner.jobs.lock().unwrap().remove(&job_id);
                Err(anyhow::Error::from(MailboxFull {
                    machine: self.id.clone(),
                }))
            }
        }
    }

    async fn status(&self, job_id: &str) -> JobState {
        self.inner
            .states
            .lock()
            .unwrap()
            .get(job_id)
            .cloned()
            .unwrap_or(JobState::Unknown)
    }

    async fn cancel(&self, job_id: &str) -> Result<()> {
        self.inner
            .cancel_tx
            .send(job_id.to_string())
            .map_err(|_| anyhow!("machine {} shut down", self.id))?;
        Ok(())
    }

    fn load(&self) -> Load {
        let m = self.inner.states.lock().unwrap();
        let running = m.values().filter(|s| **s == JobState::Running).count();
        let queued = m.values().filter(|s| **s == JobState::Queued).count();
        Load {
            running,
            queued,
            slots: self.inner.slot_count,
        }
    }

    async fn join(&self) {
        self.inner.wg.wait().await;
    }
}

/// Slot consumer body: mark running, deliver through the matching executor,
/// record the terminal state and release the WaitGroup count.
async fn run_job(inner: &MailboxInner, job: Job) {
    // A cancelled job still sitting in the mailbox must not deliver.
    if !transition(inner, &job.id, JobState::Running) {
        return;
    }
    let exec = pick_executor(&inner.executors, &job.requires);
    let outcome = match exec {
        Some(e) => e.deliver(&job).await,
        None => Err(format!(
            "no executor for requires {:?} on machine executors {:?}",
            job.requires,
            inner.executors.iter().map(|e| e.kind()).collect::<Vec<_>>()
        )),
    };
    match outcome {
        Ok(summary) => {
            if transition(inner, &job.id, JobState::Delivered) {
                inner.wg.done();
            }
            let _ = summary;
        }
        Err(err) => {
            if transition(inner, &job.id, JobState::Failed(err)) {
                inner.wg.done();
            }
        }
    }
}

/// Control-channel body for cancellation: stop the seat (context
/// cancellation) and mark the job cancelled. Transition happens BEFORE the
/// stop so a racing delivery observes the cancellation.
async fn cancel_job(inner: &MailboxInner, job_id: &str) {
    // Unknown job (never accepted / rolled back): nothing to cancel.
    let Some(job) = inner.jobs.lock().unwrap().get(job_id).cloned() else {
        return;
    };
    if !transition(inner, job_id, JobState::Cancelled) {
        return; // already terminal
    }
    let exec = inner.executors.first().cloned();
    if let Some(exec) = exec {
        // Best effort: the stop is the seat stop layers 1+2; failures are
        // not fatal because the state is already terminal.
        let _ = exec.stop(&job.seat).await;
    }
    inner.wg.done();
}

/// Guarded state transition: returns false when the job already reached a
/// terminal state (the terminal writer owns the WaitGroup count).
fn transition(inner: &MailboxInner, job_id: &str, next: JobState) -> bool {
    let mut m = inner.states.lock().unwrap();
    if matches!(m.get(job_id), Some(cur) if cur.is_terminal()) {
        return false;
    }
    m.insert(job_id.to_string(), next);
    true
}

fn pick_executor(executors: &[Arc<dyn Executor>], req: &Requires) -> Option<Arc<dyn Executor>> {
    match &req.executor {
        Some(kind) => executors.iter().find(|e| e.kind() == *kind).cloned(),
        None => executors.first().cloned(),
    }
}

// ---------------------------------------------------------------------------
// AgentMachine: pull-mode placeholder (relay F2)
// ---------------------------------------------------------------------------

/// Pull-mode machine: the agent fetches jobs via the relay heartbeat (F2).
/// Trait placeholder only — submit refuses until the heartbeat face lands.
pub struct AgentMachine {
    id: String,
    caps: Caps,
}

impl AgentMachine {
    pub fn new(id: &str, caps: Caps) -> Self {
        Self {
            id: id.to_string(),
            caps,
        }
    }
}

#[async_trait]
impl Machine for AgentMachine {
    fn id(&self) -> &str {
        &self.id
    }

    fn caps(&self) -> &Caps {
        &self.caps
    }

    fn channel(&self) -> &str {
        "agent"
    }

    async fn submit(&self, _job: Job) -> Result<JobHandle> {
        Err(anyhow!(
            "machine {} is pull-mode (agent channel); submit lands with relay F2 heartbeat",
            self.id
        ))
    }

    async fn status(&self, _job_id: &str) -> JobState {
        JobState::Unknown
    }

    async fn cancel(&self, _job_id: &str) -> Result<()> {
        Err(anyhow!("pull-mode machines hold no jobs to cancel"))
    }

    fn load(&self) -> Load {
        Load::default()
    }
}

// ---------------------------------------------------------------------------
// Pool: routing = caps filter -> least load -> mailbox submit
// ---------------------------------------------------------------------------

/// The fleet's machine set from the router's point of view.
pub struct Pool {
    machines: Vec<Arc<dyn Machine>>,
}

impl Pool {
    pub fn new(machines: Vec<Arc<dyn Machine>>) -> Self {
        Self { machines }
    }

    pub fn machines(&self) -> &[Arc<dyn Machine>] {
        &self.machines
    }

    /// Route one job: filter machines by the job's Requires, sort by load
    /// ratio (ties break by id so routing is deterministic), then submit to
    /// the first machine whose mailbox accepts. MailboxFull reroutes to the
    /// next candidate — backpressure IS the load balancer.
    pub async fn route(&self, job: Job) -> Result<JobHandle> {
        let mut cands: Vec<&Arc<dyn Machine>> = self
            .machines
            .iter()
            .filter(|m| m.caps().satisfies(&job.requires) && m.load().slots > 0)
            .collect();
        if cands.is_empty() {
            return Err(anyhow!(
                "no machine satisfies requires {:?} (job {})",
                job.requires,
                job.id
            ));
        }
        // Least load first; ties break by id (deterministic routing).
        // f32 has no Ord, but ratios are non-negative: bit patterns order
        // them totally (IEEE trick), so sort_by_key stays possible.
        cands.sort_by_key(|m| (m.load().ratio().to_bits(), m.id().to_string()));
        let mut backpressured = Vec::new();
        for m in cands {
            match m.submit(job.clone()).await {
                Ok(h) => return Ok(h),
                Err(e) => {
                    if e.downcast_ref::<MailboxFull>().is_some() {
                        backpressured.push(m.id().to_string());
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(anyhow!(
            "all candidate mailboxes full (backpressured: {})",
            backpressured.join(", ")
        ))
    }

    /// Receipt query across the pool: first machine reporting a known state.
    pub async fn status(&self, job_id: &str) -> Option<JobState> {
        for m in &self.machines {
            let s = m.status(job_id).await;
            if s != JobState::Unknown {
                return Some(s);
            }
        }
        None
    }

    /// Cancel across the pool: the machine holding the job stops the seat.
    pub async fn cancel(&self, job_id: &str) -> Result<()> {
        for m in &self.machines {
            if m.status(job_id).await != JobState::Unknown {
                return m.cancel(job_id).await;
            }
        }
        Err(anyhow!("job {job_id} not held by any machine"))
    }

    /// sync.WaitGroup mapping: wait until every machine's in-flight batch
    /// converges (split batches like boundless A/B converge here).
    pub async fn wait_all(&self) {
        for m in &self.machines {
            m.join().await;
        }
    }

    /// Snapshot for `fleetd machines` / /v1/machines. `procs` probes live
    /// processes per host channel (best effort, 0 when unreachable).
    pub fn stats(&self) -> Vec<MachineStat> {
        self.machines
            .iter()
            .map(|m| {
                let load = m.load();
                let caps = m.caps();
                let host = match m.channel() {
                    "local" => "local".to_string(),
                    c => c.strip_prefix("ssh:").unwrap_or(c).to_string(),
                };
                // Pull-mode machines hold no local processes to probe.
                let procs = if m.channel() == "agent" {
                    0
                } else {
                    agent::registry(AgentKind::Claude)
                        .probe_load(&host, load.slots)
                        .map(|l| l.running)
                        .unwrap_or(0)
                };
                MachineStat {
                    id: m.id().to_string(),
                    channel: m.channel().to_string(),
                    os: caps.os.clone(),
                    executors: caps.executors.clone(),
                    tags: caps.tags.clone(),
                    running: load.running,
                    queued: load.queued,
                    slots: load.slots,
                    procs,
                }
            })
            .collect()
    }
}

/// Build machines from fleet config: declared `[[workers]]` when present,
/// otherwise derived from legacy `hosts` (one claude executor per host —
/// the compat mapping is preserved). Executor flavor:
/// - headless: the unattended dispatch face (claude -p per task)
/// - tmux paste: interactive seat delivery (agent face)
///
/// Must run inside a tokio runtime (starts the resident tasks).
pub fn build_machines(cfg: &FleetConfig, tmux_paste: bool) -> Vec<Arc<dyn Machine>> {
    cfg.workers()
        .iter()
        .map(|w| {
            let caps = Caps {
                executors: w.executors.iter().map(|e| e.kind.clone()).collect(),
                tags: w.tags.clone(),
                os: w.os.clone(),
            };
            let slots = w.max_concurrent().max(1);
            if w.channel == "agent" {
                return Arc::new(AgentMachine::new(&w.name, caps)) as Arc<dyn Machine>;
            }
            let host = if w.channel == "local" {
                "local".to_string()
            } else {
                w.channel
                    .strip_prefix("ssh:")
                    .unwrap_or(&w.channel)
                    .to_string()
            };
            let channel = if w.channel == "local" {
                "local".to_string()
            } else {
                w.channel.clone()
            };
            let executors: Vec<Arc<dyn Executor>> = if tmux_paste {
                vec![Arc::new(TmuxExecutor::new(&host))]
            } else {
                // Headless faces need the host's workdir/scratch facts; the
                // legacy hosts entry carries them (derived workers inherit).
                let h = cfg.hosts.iter().find(|h| h.name == w.name);
                let (os, workdir, scratch) = match h {
                    Some(h) => (h.os.clone(), h.workdir.clone(), h.scratch_dir()),
                    None => (w.os.clone(), String::new(), String::new()),
                };
                vec![Arc::new(HeadlessExecutor {
                    host: host.clone(),
                    os,
                    workdir,
                    scratch,
                })]
            };
            MailboxMachine::with_executors(&w.name, caps, slots, executors, channel)
                as Arc<dyn Machine>
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Scheduler: the select loop
// ---------------------------------------------------------------------------

/// The scheduler select loop: new jobs / events / reconcile ticker — the
/// `select` arm of the CSP mapping. Handlers are injected so the server
/// plane wires its own event face without core depending on it.
pub struct Scheduler {
    pub pool: Pool,
    pending_rx: mpsc::Receiver<Job>,
    event_rx: mpsc::Receiver<String>,
    on_event: Arc<dyn Fn(&str) + Send + Sync>,
    on_reconcile: Arc<dyn Fn() + Send + Sync>,
}

impl Scheduler {
    pub fn new(
        pool: Pool,
        pending_rx: mpsc::Receiver<Job>,
        event_rx: mpsc::Receiver<String>,
        on_event: Arc<dyn Fn(&str) + Send + Sync>,
        on_reconcile: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            pool,
            pending_rx,
            event_rx,
            on_event,
            on_reconcile,
        }
    }

    /// Run until both the pending and event sources close. The ticker keeps
    /// firing regardless; reconcile is the safety net, events are primary
    /// (mailbox consumers + Stop hook feed the state, the loop only reacts).
    pub async fn run(mut self, tick_interval: std::time::Duration) {
        let mut ticker = tokio::time::interval(tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut pending_open = true;
        let mut events_open = true;
        loop {
            tokio::select! {
                job = self.pending_rx.recv(), if pending_open => match job {
                    Some(job) => {
                        if let Err(e) = self.pool.route(job).await {
                            eprintln!("fleetd machine scheduler: route failed: {e}");
                        }
                    }
                    None => pending_open = false,
                },
                ev = self.event_rx.recv(), if events_open => match ev {
                    Some(ev) => (self.on_event)(&ev),
                    None => events_open = false,
                },
                // Ticker only runs while a source lives; once both close,
                // every arm is disabled and the `else` arm ends the loop.
                _ = ticker.tick(), if pending_open || events_open => (self.on_reconcile)(),
                else => return, // every source closed
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: mock unit tests for the five required behaviors + extras
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Mock executor: records deliveries and stops; `hold` makes deliver
    /// park until released (drives backpressure deterministically).
    struct MockExecutor {
        name: &'static str,
        shared: Arc<MockShared>,
    }

    #[derive(Default)]
    struct MockShared {
        hold: Mutex<bool>,
        notify: Notify,
        delivered: Mutex<Vec<String>>,
        stopped: Mutex<Vec<String>>,
    }

    impl MockShared {
        fn release(&self) {
            *self.hold.lock().unwrap() = false;
            self.notify.notify_waiters();
        }
    }

    #[async_trait]
    impl Executor for MockExecutor {
        fn kind(&self) -> String {
            self.name.to_string()
        }

        async fn deliver(&self, job: &Job) -> std::result::Result<String, String> {
            loop {
                if !*self.shared.hold.lock().unwrap() {
                    break;
                }
                let notified = self.shared.notify.notified();
                if !*self.shared.hold.lock().unwrap() {
                    break;
                }
                notified.await;
            }
            self.shared.delivered.lock().unwrap().push(job.id.clone());
            Ok(format!("mock delivered {}", job.id))
        }

        async fn stop(&self, seat: &str) -> std::result::Result<(), String> {
            self.shared.stopped.lock().unwrap().push(seat.to_string());
            Ok(())
        }
    }

    fn mock_machine(
        id: &str,
        slots: usize,
        tags: &[&str],
        os: &str,
    ) -> (Arc<MailboxMachine>, Arc<MockShared>) {
        let shared = Arc::new(MockShared::default());
        // Deliveries park until the test releases them (deterministic).
        *shared.hold.lock().unwrap() = true;
        let exec: Arc<dyn Executor> = Arc::new(MockExecutor {
            name: "claude",
            shared: Arc::clone(&shared),
        });
        let caps = Caps {
            executors: vec!["claude".into()],
            tags: tags.iter().map(|t| t.to_string()).collect(),
            os: os.into(),
        };
        let m = MailboxMachine::with_executors(
            id,
            caps,
            slots,
            vec![exec],
            if id == "local" {
                "local".into()
            } else {
                format!("ssh:{id}")
            },
        );
        (m, shared)
    }

    fn job(id: &str, seat: &str) -> Job {
        Job {
            id: id.into(),
            seat: seat.into(),
            tag: id.into(),
            text: format!("task {id}"),
            ..Default::default()
        }
    }

    async fn until(mut tries: usize, mut f: impl AsyncFnMut() -> bool) -> bool {
        while tries > 0 {
            if f().await {
                return true;
            }
            tries -= 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        f().await
    }

    /// Wait until the machine reports the job in `want` (drives the drainer
    /// forward on the current-thread runtime before the next route step).
    async fn wait_state(m: &MailboxMachine, id: &str, want: JobState) {
        assert!(
            until(300, async || m.status(id).await == want).await,
            "job {id} never reached {:?} (is {:?})",
            want,
            m.status(id).await
        );
    }

    // (1) backpressure: saturate both machines; the overflow submit gets
    // MailboxFull from every candidate -> route errors (and any machine
    // with room takes the reroute).
    #[tokio::test]
    async fn backpressure_full_mailbox_reroutes() {
        let (a, held_a) = mock_machine("local", 1, &["linux"], "linux");
        let (b, held_b) = mock_machine("remote-b", 1, &["linux"], "linux");
        let pool = Pool::new(vec![a.clone(), b.clone()]);

        // j1 -> local (tie broken by id); wait until it actually occupies
        // the slot so load reads are stable.
        let h1 = pool.route(job("j1", "seat-x")).await.unwrap();
        assert_eq!(h1.machine_id, "local");
        wait_state(a.as_ref(), "j1", JobState::Running).await;
        // j2 sees local loaded -> reroutes to the idle peer.
        let h2 = pool.route(job("j2", "seat-x")).await.unwrap();
        assert_eq!(h2.machine_id, "remote-b");
        wait_state(b.as_ref(), "j2", JobState::Running).await;
        // j3/j4 fill both mailboxes (cap = slots = 1 queued each).
        let h3 = pool.route(job("j3", "seat-x")).await.unwrap();
        let h4 = pool.route(job("j4", "seat-x")).await.unwrap();
        assert_eq!(h3.machine_id, "local"); // tie -> id order
        assert_eq!(h4.machine_id, "remote-b");
        // j5: every mailbox is full -> backpressure reported, nothing lost.
        let err = pool.route(job("j5", "seat-x")).await.unwrap_err();
        assert!(err.to_string().contains("all candidate mailboxes full"));
        assert_eq!(a.status("j5").await, JobState::Unknown);
        assert_eq!(b.status("j5").await, JobState::Unknown);

        held_a.release();
        held_b.release();
        pool.wait_all().await;
        assert_eq!(a.status("j1").await, JobState::Delivered);
        assert_eq!(a.status("j3").await, JobState::Delivered);
        assert_eq!(b.status("j2").await, JobState::Delivered);
        assert_eq!(b.status("j4").await, JobState::Delivered);
    }

    // (2) cancel: submitted job cancelled -> seat stopped, state cancelled.
    #[tokio::test]
    async fn cancel_stops_seat_and_records_state() {
        let (m, shared) = mock_machine("local", 1, &[], "linux");
        let pool = Pool::new(vec![m.clone()]);

        pool.route(job("j1", "seat-y")).await.unwrap();
        // Wait until the job is mid-delivery (held), then cancel it.
        wait_state(m.as_ref(), "j1", JobState::Running).await;
        pool.cancel("j1").await.unwrap();
        assert!(
            until(200, async || m.status("j1").await == JobState::Cancelled).await,
            "job must reach Cancelled"
        );
        // The cancel leg is the seat stop (layers 1+2): recorded by the mock.
        assert!(
            until(200, async || shared
                .stopped
                .lock()
                .unwrap()
                .contains(&"seat-y".to_string()))
            .await,
            "seat must receive the stop"
        );
        // WaitGroup released by the cancel leg: batch converges.
        tokio::time::timeout(Duration::from_secs(2), pool.wait_all())
            .await
            .expect("cancelled job must release the WaitGroup");
    }

    // (3) status readback: queued -> running -> delivered lifecycle.
    #[tokio::test]
    async fn status_readback_lifecycle() {
        let (m, shared) = mock_machine("local", 1, &[], "linux");
        let j = job("j1", "seat-z");
        let h = m.submit(j).await.unwrap();
        assert_eq!(h.machine_id, "local");
        assert_eq!(m.status("j1").await, JobState::Queued);
        // Mid-delivery the state reads running (held mock keeps it there).
        wait_state(m.as_ref(), "j1", JobState::Running).await;
        shared.release();
        assert!(
            until(200, async || m.status("j1").await == JobState::Delivered).await,
            "terminal state must be readable"
        );
        // Unknown ids read Unknown, never panic.
        assert_eq!(m.status("nope").await, JobState::Unknown);
    }

    // (4) constraint filter: Requires routes by caps, unsatisfiable errors.
    #[tokio::test]
    async fn constraint_filter_routes_by_caps() {
        let (linux_m, _s1) = mock_machine("linux-box", 1, &["linux"], "linux");
        let (win_m, _s2) = mock_machine("win-box", 1, &["desktop"], "windows");
        let pool = Pool::new(vec![linux_m.clone(), win_m.clone()]);

        let mut j = job("jw", "seat-ui");
        j.requires = Requires {
            executor: Some("claude".into()),
            tags: vec!["desktop".into()],
            os: Some("windows".into()),
        };
        let h = pool.route(j).await.unwrap();
        assert_eq!(h.machine_id, "win-box");

        // Unsatisfiable requires: no machine carries a shell executor.
        let mut j2 = job("js", "seat-sh");
        j2.requires.executor = Some("shell".into());
        let err = pool.route(j2).await.unwrap_err();
        assert!(err.to_string().contains("no machine satisfies"));
    }

    // (5) dual-machine load balance: least-loaded machine wins each send.
    #[tokio::test]
    async fn dual_machine_load_balance() {
        let (a, held_a) = mock_machine("local", 1, &["linux"], "linux");
        let (b, held_b) = mock_machine("remote-b", 1, &["linux"], "linux");
        let pool = Pool::new(vec![a.clone(), b.clone()]);

        // Both held: the first job takes local (tie), the second sees
        // local loaded and reroutes to the idle peer.
        let h1 = pool.route(job("j1", "s")).await.unwrap();
        wait_state(a.as_ref(), "j1", JobState::Running).await;
        let h2 = pool.route(job("j2", "s")).await.unwrap();
        assert_eq!(h1.machine_id, "local");
        assert_eq!(h2.machine_id, "remote-b");
        wait_state(b.as_ref(), "j2", JobState::Running).await;
        assert_eq!(a.load().running, 1);
        assert_eq!(b.load().running, 1);
        held_a.release();
        held_b.release();
        pool.wait_all().await;
        assert_eq!(a.status("j1").await, JobState::Delivered);
        assert_eq!(b.status("j2").await, JobState::Delivered);
    }

    // (6) WaitGroup: batch convergence counting (split batches converge).
    #[tokio::test]
    async fn wait_group_batch_convergence() {
        let wg = Arc::new(WaitGroup::new());
        wg.add(3);
        for _ in 0..3 {
            let wg = Arc::clone(&wg);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                wg.done();
            });
        }
        tokio::time::timeout(Duration::from_secs(2), wg.wait())
            .await
            .expect("batch must converge");
    }

    // (7) AgentMachine stays a placeholder until relay F2 lands.
    #[tokio::test]
    async fn agent_machine_refuses_submit() {
        let m = AgentMachine::new(
            "ship-1",
            Caps {
                executors: vec!["claude".into()],
                tags: vec![],
                os: "linux".into(),
            },
        );
        let err = m.submit(job("j1", "s")).await.unwrap_err();
        assert!(err.to_string().contains("relay F2"));
        assert_eq!(m.channel(), "agent");
        assert_eq!(m.load(), Load::default());
    }

    // (8) Scheduler select loop: pending jobs route, ticker reconciles,
    // closing sources ends the loop (no busy-spin on closed channels).
    #[tokio::test]
    async fn scheduler_select_loop_routes_and_exits() {
        let (a, held) = mock_machine("local", 1, &[], "linux");
        let (ptx, prx) = mpsc::channel::<Job>(4);
        let (etx, erx) = mpsc::channel::<String>(4);
        let events = Arc::new(Mutex::new(Vec::new()));
        let reconciles = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let ev_log = Arc::clone(&events);
        let rc = Arc::clone(&reconciles);
        let sched = Scheduler::new(
            Pool::new(vec![a.clone()]),
            prx,
            erx,
            Arc::new(move |e: &str| ev_log.lock().unwrap().push(e.to_string())),
            Arc::new(move || {
                rc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
        );
        // The loop runs on its own task; the test drives it through the
        // channel handles.
        let loop_task = tokio::spawn(sched.run(Duration::from_millis(20)));

        ptx.send(job("j1", "s")).await.unwrap();
        etx.send("stop-hook seat=s".into()).await.unwrap();
        // Let the ticker fire a couple of rounds while sources stay open,
        // then drop both: the loop exits through the `else` arm.
        tokio::time::sleep(Duration::from_millis(60)).await;
        drop(ptx);
        drop(etx);
        tokio::time::timeout(Duration::from_secs(3), loop_task)
            .await
            .expect("loop must exit when sources close")
            .expect("loop task must not panic");
        assert_eq!(events.lock().unwrap().len(), 1);
        assert!(
            reconciles.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "reconcile ticker must have fired"
        );
        held.release();
        assert!(
            until(200, async || a.status("j1").await == JobState::Delivered).await,
            "the loop must have routed the pending job"
        );
    }

    // (9) config mapping: legacy hosts derive machines (channels + slots
    // preserved); declared workers win, pull-mode stays a placeholder.
    #[tokio::test]
    async fn build_machines_from_config_maps_channels() {
        // Legacy hosts only: the compat mapping derives one machine per host.
        let hosts_only: FleetConfig = toml::from_str(
            r#"
[[hosts]]
name = "local"
ssh = "local"
max_concurrent = 2
os = "linux"
[[hosts]]
name = "imac"
ssh = "imac"
max_concurrent = 3
os = "windows"
"#,
        )
        .unwrap();
        let ms = build_machines(&hosts_only, true);
        assert_eq!(ms.len(), 2);
        let by_id =
            |ms: &[Arc<dyn Machine>], id: &str| ms.iter().find(|m| m.id() == id).unwrap().clone();
        assert_eq!(by_id(&ms, "local").channel(), "local");
        assert_eq!(by_id(&ms, "imac").channel(), "ssh:imac");
        assert_eq!(by_id(&ms, "local").load().slots, 2);
        assert_eq!(by_id(&ms, "imac").load().slots, 3);
        // Declared workers replace derived ones (worker-design.md).
        let declared: FleetConfig = toml::from_str(
            r#"
[[hosts]]
name = "local"
ssh = "local"
max_concurrent = 2
os = "linux"
[[workers]]
name = "ship"
channel = "agent"
os = "linux"
executors = [{ kind = "claude", max_concurrent = 1 }]
"#,
        )
        .unwrap();
        let ms2 = build_machines(&declared, false);
        assert_eq!(ms2.len(), 1);
        assert_eq!(by_id(&ms2, "ship").channel(), "agent");
        assert_eq!(by_id(&ms2, "ship").load().slots, 0); // pull mode: no slots
    }

    // (10) caps satisfaction corner cases (pure).
    #[test]
    fn caps_satisfaction_matrix() {
        let caps = Caps {
            executors: vec!["claude".into(), "shell".into()],
            tags: vec!["linux".into(), "desktop".into()],
            os: "linux".into(),
        };
        assert!(caps.satisfies(&Requires::default()));
        assert!(caps.satisfies(&Requires {
            executor: Some("shell".into()),
            tags: vec!["desktop".into()],
            os: None,
        }));
        assert!(!caps.satisfies(&Requires {
            executor: Some("ahk-ui".into()),
            tags: vec![],
            os: None,
        }));
        assert!(!caps.satisfies(&Requires {
            executor: None,
            tags: vec!["gpu".into()],
            os: None,
        }));
        assert!(!caps.satisfies(&Requires {
            executor: None,
            tags: vec![],
            os: Some("macos".into()),
        }));
    }
}
