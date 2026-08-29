use std::{
  any::Any,
  fmt::Debug,
  ops::Deref,
  sync::{Arc, RwLock, atomic::AtomicUsize},
};

use tokio::sync::mpsc::UnboundedSender;

use crate::term::Screen;

use super::sub_trie::SubMode;
use super::task::{ExitInfo, Task, TaskCmd, TaskDef, TaskId, TaskState};
use super::task_key::{TaskKey, TaskSpaceId};
use super::task_path::TaskPath;

pub struct KernelMessage {
  pub from: TaskId,
  pub command: KernelCommand,
}

pub struct TaskRegistration {
  pub task_id: TaskId,
  pub def: TaskDef,
  pub factory: Box<dyn FnOnce(TaskContext) -> Box<dyn Task> + Send>,
}

impl TaskRegistration {
  pub fn async_task<F, Fut>(task_id: TaskId, def: TaskDef, f: F) -> Self
  where
    F: FnOnce(TaskContext, tokio::sync::mpsc::UnboundedReceiver<TaskCmd>) -> Fut
      + Send
      + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
  {
    use super::task::ChannelTask;
    Self {
      task_id,
      def,
      factory: Box::new(|ctx| {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(f(ctx, rx));
        Box::new(ChannelTask::new(tx))
      }),
    }
  }
}

pub enum KernelCommand {
  Quit,

  RegisterTask(TaskRegistration, tokio::sync::oneshot::Sender<bool>),
  /// Total: removes a task in any state, killing it if it is running.
  RemoveTask(TaskId),

  /// Intent commands resolve the selector and act on the matches in the
  /// same dispatch, so no other message can interleave between the two.
  /// The ack is answered in that dispatch with the matched-task count.
  Start(TaskSelector, Option<tokio::sync::oneshot::Sender<usize>>),
  Stop(TaskSelector, Option<tokio::sync::oneshot::Sender<usize>>),
  Kill(TaskSelector, Option<tokio::sync::oneshot::Sender<usize>>),
  Restart(TaskSelector, Option<tokio::sync::oneshot::Sender<usize>>),
  ForceRestart(TaskSelector, Option<tokio::sync::oneshot::Sender<usize>>),
  Down(TaskSelector, Option<tokio::sync::oneshot::Sender<usize>>),
  Veto(TaskSelector, Option<tokio::sync::oneshot::Sender<usize>>),
  /// `from` requires `to`.
  AddEdge {
    from: TaskId,
    to: TaskId,
  },
  RemoveEdge {
    from: TaskId,
    to: TaskId,
  },

  TaskMsg(TaskId, Box<dyn Any + Send>),

  SetTaskPath(TaskId, TaskPath),
  SetTaskLabel(TaskId, Option<String>),

  Query(
    KernelQuery,
    tokio::sync::oneshot::Sender<KernelQueryResponse>,
  ),

  SubscribePath(TaskKey, SubMode),
  UnsubscribePath(TaskKey, SubMode),
  /// Sends `true`/`false` whenever the selected set gains its first active
  /// task or loses its last one.
  WatchActive(TaskSelector, UnboundedSender<bool>),

  // Task reporting
  TaskStarted,
  TaskReady,
  TaskStopped(ExitInfo),

  /// A time limit set on the task's current state ran out (stop grace,
  /// backoff delay). The epoch says which state it was set for, so a
  /// timeout from an earlier state is ignored.
  StateTimeout(TaskId, u64),
}

#[derive(Clone, Debug)]
pub enum TaskSelector {
  Id(TaskId),
  /// Every task with a path.
  All(TaskSpaceId),
  Glob(TaskSpaceId, String),
  /// Tasks carrying the tag.
  Tag(TaskSpaceId, String),
}

pub enum KernelQuery {
  /// List tasks matching an optional glob. None = list all.
  ListTasks(TaskSpaceId, Option<String>),
  /// Resolve a path to a TaskId.
  ResolvePath(TaskKey),
  /// List the task ids carrying a tag.
  TasksWithTag(TaskSpaceId, String),
  /// Get the current screen content for a task (rendered as ANSI text).
  GetScreen(TaskKey),
  /// Explain why a task is (not) running.
  Explain(TaskKey),
}

pub enum KernelQueryResponse {
  TaskList(Vec<TaskInfo>),
  ResolvedPath(Option<TaskId>),
  TaggedTasks(Vec<TaskId>),
  /// ANSI-rendered screen content, or None if the task has no screen.
  Screen(Option<String>),
  Explain(Option<TaskExplain>),
}

#[derive(Clone, Debug)]
pub struct TaskInfo {
  pub id: TaskId,
  pub space: TaskSpaceId,
  pub path: Option<TaskPath>,
  pub label: Option<String>,
  pub state: TaskState,
  pub vt: Option<SharedVt>,
}

#[derive(Clone, Debug)]
pub struct TaskExplain {
  pub state: TaskState,
  pub wanted: bool,
  /// Wanted and every dependency transitively supported and satisfied;
  /// false on a wanted task means it is blocked by a dep below.
  pub supported: bool,
  pub vetoed: bool,
  pub pinned: bool,
  pub required_by: Vec<String>,
  pub deps: Vec<DepExplain>,
  pub attempts: u32,
}

#[derive(Clone, Debug)]
pub struct DepExplain {
  pub name: String,
  pub state: TaskState,
  pub wanted: bool,
  pub satisfied: bool,
}

#[derive(Clone)]
pub struct SharedVt(Arc<RwLock<Screen>>);

impl SharedVt {
  pub fn new(screen: Screen) -> Self {
    SharedVt(Arc::new(RwLock::new(screen)))
  }
}

impl Debug for SharedVt {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_tuple("SharedVt").finish()
  }
}

impl Deref for SharedVt {
  type Target = Arc<RwLock<Screen>>;

  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

#[derive(Clone)]
pub struct TaskContext {
  next_task_id: Arc<AtomicUsize>,
  sender: UnboundedSender<KernelMessage>,
  pub task_id: TaskId,
}

impl TaskContext {
  pub fn new(
    next_task_id: Arc<AtomicUsize>,
    task_id: TaskId,
    sender: UnboundedSender<KernelMessage>,
  ) -> Self {
    Self {
      next_task_id,
      sender,
      task_id,
    }
  }

  pub fn send(&self, command: KernelCommand) {
    if let Err(_err) = self.sender.send(KernelMessage {
      from: self.task_id,
      command,
    }) {
      log::debug!(
        "Failed to send kernel message (task_id: {}). Channel is closed.",
        self.task_id.0,
      );
    }
  }

  pub fn send_msg<T: Any + Send + 'static>(&self, to: TaskId, msg: T) {
    self.send(KernelCommand::TaskMsg(to, Box::new(msg)));
  }

  pub fn send_self_custom<T: Any + Send + 'static>(&self, custom: T) {
    self.send_msg(self.task_id, custom);
  }

  pub fn alloc_id(&self) -> TaskId {
    TaskId(
      self
        .next_task_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    )
  }

  pub fn register(
    &self,
    def: TaskDef,
    factory: Box<dyn FnOnce(TaskContext) -> Box<dyn Task> + Send>,
  ) -> TaskId {
    let task_id = self.alloc_id();
    self.register_with_id(task_id, def, factory)
  }

  pub fn register_with_id(
    &self,
    task_id: TaskId,
    def: TaskDef,
    factory: Box<dyn FnOnce(TaskContext) -> Box<dyn Task> + Send>,
  ) -> TaskId {
    let _ = self.register_task(TaskRegistration {
      task_id,
      def,
      factory,
    });
    task_id
  }

  pub fn spawn_async<F, Fut>(&self, def: TaskDef, f: F) -> TaskId
  where
    F: FnOnce(TaskContext, tokio::sync::mpsc::UnboundedReceiver<TaskCmd>) -> Fut
      + Send
      + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
  {
    let task_id = self.alloc_id();
    let _ = self.spawn_async_with_id(task_id, def, f);
    task_id
  }

  /// The returned ack resolves to whether the task was registered.
  pub fn spawn_async_with_id<F, Fut>(
    &self,
    task_id: TaskId,
    def: TaskDef,
    f: F,
  ) -> tokio::sync::oneshot::Receiver<bool>
  where
    F: FnOnce(TaskContext, tokio::sync::mpsc::UnboundedReceiver<TaskCmd>) -> Fut
      + Send
      + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
  {
    let registration = TaskRegistration::async_task(task_id, def, f);
    self.register_task(registration)
  }

  pub fn register_task(
    &self,
    registration: TaskRegistration,
  ) -> tokio::sync::oneshot::Receiver<bool> {
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    self.send(KernelCommand::RegisterTask(registration, ack_tx));
    ack_rx
  }

  pub fn set_task_path(&self, task_id: TaskId, path: TaskPath) {
    self.send(KernelCommand::SetTaskPath(task_id, path));
  }

  pub fn set_task_label(&self, task_id: TaskId, label: Option<String>) {
    self.send(KernelCommand::SetTaskLabel(task_id, label));
  }

  pub fn subscribe_path(&self, key: TaskKey, mode: SubMode) {
    self.send(KernelCommand::SubscribePath(key, mode));
  }

  pub fn watch_active(
    &self,
    selector: TaskSelector,
  ) -> tokio::sync::mpsc::UnboundedReceiver<bool> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    self.send(KernelCommand::WatchActive(selector, tx));
    rx
  }

  pub fn unsubscribe_path(&self, key: TaskKey, mode: SubMode) {
    self.send(KernelCommand::UnsubscribePath(key, mode));
  }

  pub fn query(
    &self,
    query: KernelQuery,
  ) -> tokio::sync::oneshot::Receiver<KernelQueryResponse> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    self.send(KernelCommand::Query(query, tx));
    rx
  }

  pub fn get_task_sender(&self, target_id: TaskId) -> TaskSender {
    TaskSender {
      task_id: target_id,
      from_id: self.task_id,
      sender: self.sender.clone(),
    }
  }
}

#[derive(Clone)]
pub struct TaskSender {
  pub task_id: TaskId,
  pub from_id: TaskId,
  sender: UnboundedSender<KernelMessage>,
}

impl TaskSender {
  pub fn send<T: Any + Send + 'static>(&self, msg: T) {
    let r = self.sender.send(KernelMessage {
      from: self.from_id,
      command: KernelCommand::TaskMsg(self.task_id, Box::new(msg)),
    });
    if let Err(_err) = r {
      log::debug!(
        "TaskSender.send() to closed channel. from_id:{} task_id:{}",
        self.from_id.0,
        self.task_id.0
      );
    }
  }
}
