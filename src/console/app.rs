use tokio::sync::{mpsc::UnboundedReceiver, oneshot};

use crate::{
  command::{Command, Target, issue},
  config::{
    config::Config,
    task::{CmdConfig, TaskConfig},
  },
  console::{
    action::{Action, CopyMove, ScrollUnit},
    app_layout::AppLayout,
    client::ClientHandle,
    client::{ClientEvent, ClientId},
    keymap::Keymap,
    modal::{
      add_task::AddTaskModal,
      commands_menu::CommandsMenuModal,
      modal::{Modal, ModalResult},
      quit::QuitModal,
      remove_task::RemoveTaskModal,
      rename_task::RenameTaskModal,
    },
    state::{Scope, State},
    task_view::TaskView,
    ui_keymap::render_keymap,
    ui_tasks::{render_tasks, task_at},
    ui_term::render_term,
    ui_zoom_tip::render_zoom_tip,
    widgets::list::ListState,
  },
  error::ResultLogger,
  kernel::{
    copy_mode::CopyMove as KernelCopyMove,
    kernel_message::{KernelCommand, SharedVt, TaskContext, TaskRegistration},
    sub_trie::SubMode,
    task::{
      ChannelTask, TaskCmd, TaskDef, TaskId, TaskNotification, TaskNotify,
      TaskState,
    },
    task_key::{TaskKey, TaskSpaceId},
    task_path::TaskPath,
    task_screen::{
      FramedScreenNotify, ScrollUnit as KernelScrollUnit, TaskScreenCmd,
    },
  },
  protocol::{Bye, CtlMsg, codes},
  task::{
    config_tasks::{spawn_config_task, unique_task_name},
    process_task::{DuplicateTask, ProcessInput, ProcessPaste},
  },
  term::{
    CursorStyle, Grid, Size, TermEvent, Winsize,
    attrs::Attrs,
    grid::Rect,
    key::{Key, KeyEventKind},
    mouse::{MouseButton, MouseEventKind},
  },
};

fn kernel_copy_move(dir: CopyMove) -> KernelCopyMove {
  match dir {
    CopyMove::Up => KernelCopyMove::Up,
    CopyMove::Down => KernelCopyMove::Down,
    CopyMove::Left => KernelCopyMove::Left,
    CopyMove::Right => KernelCopyMove::Right,
  }
}

fn kernel_scroll_unit(unit: ScrollUnit) -> KernelScrollUnit {
  match unit {
    ScrollUnit::Line => KernelScrollUnit::Line,
    ScrollUnit::HalfScreen => KernelScrollUnit::HalfScreen,
    ScrollUnit::Screen => KernelScrollUnit::Screen,
  }
}

fn winsize(size: Size) -> Winsize {
  Winsize {
    x: size.width,
    y: size.height,
    x_px: 0,
    y_px: 0,
  }
}

/// The receiver resolves once the console has said goodbye to its clients.
pub fn console_task_registration(
  task_id: TaskId,
  def: TaskDef,
  config: Config,
  keymap: Keymap,
) -> (TaskRegistration, oneshot::Receiver<()>) {
  let (done, done_rx) = oneshot::channel();
  let registration = TaskRegistration {
    task_id,
    def,
    factory: Box::new(move |pc| {
      log::debug!("Creating console task (id: {})", pc.task_id.0);
      // Subscribe at registration, so tasks registered later are seen
      // before any message addressed to the console.
      pc.subscribe_path(
        TaskKey::default_space(TaskPath::root()),
        SubMode::Subtree,
      );
      let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
      tokio::spawn(async move {
        App::new(config, keymap, rx, pc).run().await;
        let _ = done.send(());
      });
      Box::new(ChannelTask::new(tx))
    }),
  };
  (registration, done_rx)
}

pub struct App {
  config: Config,
  keymap: Keymap,
  state: State,
  grid: Grid,
  modal: Option<Box<dyn Modal>>,
  receiver: UnboundedReceiver<TaskCmd>,
  pc: TaskContext,
  clients: Vec<ClientHandle>,
  stop: bool,
}

impl App {
  fn new(
    config: Config,
    keymap: Keymap,
    receiver: UnboundedReceiver<TaskCmd>,
    pc: TaskContext,
  ) -> Self {
    let size = Size {
      width: 160,
      height: 50,
    };
    let grid = Grid::new(size, 0);
    App {
      state: State {
        scope: Scope::Tasks,
        tasks: Vec::new(),
        tasks_list: ListState::default(),
        hide_keymap_window: !config.tui.tips.show,
        quitting: false,
      },
      config,
      keymap,
      grid,
      modal: None,
      receiver,
      pc,
      clients: Vec::new(),
      stop: false,
    }
  }

  async fn run(mut self) {
    let mut term_size = self.layout().term_area().size();
    let mut batch = Vec::new();
    while !self.stop {
      let size = self.layout().term_area().size();
      if size != term_size {
        term_size = size;
        for task in &self.state.tasks {
          self.pc.send_msg(
            task.id,
            TaskScreenCmd::Resize {
              size: winsize(size),
              observer_id: self.pc.task_id,
            },
          );
        }
      }

      // Zero means the kernel is gone.
      if self.receiver.recv_many(&mut batch, 512).await == 0 {
        break;
      }
      for cmd in batch.drain(..) {
        self.handle_task_cmd(cmd);
      }

      if !self.clients.is_empty() {
        self.render().await;
      }
    }

    for client in &mut self.clients {
      let bye = Bye {
        code: codes::QUIT.to_string(),
        message: String::new(),
      };
      client.sender.send_ctl(CtlMsg::Bye(bye)).await.log_ignore();
    }
    if !self.clients.is_empty() {
      self.unobserve_all();
    }
    self.pc.unsubscribe_path(
      TaskKey::default_space(TaskPath::root()),
      SubMode::Subtree,
    );
  }

  async fn render(&mut self) {
    let layout = self.layout();
    let grid = &mut self.grid;
    grid.erase_all(Attrs::default());
    grid.cursor_pos = None;
    grid.cursor_style = CursorStyle::Default;

    render_tasks(layout.sidebar, grid, &mut self.state, &self.config);
    render_term(layout.term, grid, &self.state);
    render_keymap(layout.keymap, grid, &self.state, &self.keymap);
    render_zoom_tip(layout.zoom_banner, grid, &self.keymap);
    if let Some(modal) = &mut self.modal {
      grid.cursor_pos = None;
      grid.cursor_style = CursorStyle::Default;
      modal.render(grid, &self.keymap);
    }

    for client in &mut self.clients {
      let mut out = Vec::new();
      client.differ.diff(&mut out, grid);
      client.sender.send_out(out.into()).await.log_ignore();
    }
  }

  fn layout(&self) -> AppLayout {
    let size = self.grid.size();
    AppLayout::new(
      Rect::new(0, 0, size.width, size.height),
      self.state.scope.is_zoomed(),
      self.state.hide_keymap_window,
      &self.config,
    )
  }

  /// The grid fits the smallest client.
  fn fit_grid(&mut self) {
    let size = self.clients.iter().map(|c| c.size).reduce(|a, b| Size {
      width: a.width.min(b.width),
      height: a.height.min(b.height),
    });
    if let Some(size) = size {
      self.grid.set_size(size);
    }
  }

  fn add_task(
    &mut self,
    id: TaskId,
    label: Option<String>,
    path: Option<TaskPath>,
    status: TaskState,
    vt: Option<SharedVt>,
  ) {
    let Some(vt) = vt else {
      return;
    };
    self.state.tasks.push(TaskView {
      id,
      label,
      path,
      status,
      vt,
      present: None,
    });
    if !self.clients.is_empty() {
      self.observe(id);
    }
  }

  // Screens are observed only while someone is attached, so an idle
  // server does not process every task's output.
  fn observe(&self, id: TaskId) {
    self.pc.send_msg(
      id,
      TaskScreenCmd::Observe {
        size: winsize(self.layout().term_area().size()),
        sender: self.pc.get_task_sender(self.pc.task_id),
      },
    );
  }

  fn observe_all(&self) {
    for task in &self.state.tasks {
      self.observe(task.id);
    }
  }

  fn unobserve_all(&mut self) {
    for task in &mut self.state.tasks {
      task.present = None;
      self.pc.send_msg(
        task.id,
        TaskScreenCmd::Unobserve {
          observer_id: self.pc.task_id,
        },
      );
    }
  }

  fn remove_client(&mut self, client_id: ClientId) {
    self.clients.retain(|c| c.id != client_id);
    if self.clients.is_empty() {
      self.unobserve_all();
    }
    self.fit_grid();
  }

  fn handle_task_cmd(&mut self, cmd: TaskCmd) {
    let msg = match cmd {
      TaskCmd::Start => return,
      TaskCmd::Stop | TaskCmd::Kill => {
        self.stop = true;
        return;
      }
      TaskCmd::Msg(msg) => msg,
    };
    let msg = match msg.downcast::<Action>() {
      Ok(action) => return self.handle_action(*action),
      Err(msg) => msg,
    };
    let msg = match msg.downcast::<ClientEvent>() {
      Ok(msg) => return self.handle_client(*msg),
      Err(msg) => msg,
    };
    let msg = match msg.downcast::<FramedScreenNotify>() {
      Ok(notify) => return self.handle_screen_notify(*notify),
      Err(msg) => msg,
    };
    match msg.downcast::<TaskNotification>() {
      Ok(n) => self.handle_task_notify(n.from, n.notify),
      Err(_) => log::error!("Console received unknown message"),
    }
  }

  fn handle_client(&mut self, msg: ClientEvent) {
    match msg {
      ClientEvent::Input { client_id, event } => {
        self.handle_input(client_id, event);
      }
      ClientEvent::Connected { handle } => {
        self.clients.push(handle);
        self.fit_grid();
        if self.clients.len() == 1 {
          self.observe_all();
        }
      }
      ClientEvent::Disconnected { client_id } => self.remove_client(client_id),
    }
  }

  fn handle_input(&mut self, client_id: ClientId, event: TermEvent) {
    match event {
      TermEvent::Key(key) => self.handle_key(client_id, key),
      TermEvent::Mouse(mouse) => {
        if self.modal.is_some() {
          return;
        }
        let layout = self.layout();
        let (x, y) = (mouse.x as u16, mouse.y as u16);
        let pressed = match mouse.kind {
          MouseEventKind::Down(_) => true,
          MouseEventKind::Up(_)
          | MouseEventKind::Drag(_)
          | MouseEventKind::Moved
          | MouseEventKind::ScrollDown
          | MouseEventKind::ScrollUp
          | MouseEventKind::ScrollLeft
          | MouseEventKind::ScrollRight => false,
        };
        let term_area = layout.term_area();
        if term_area.contains(x, y) {
          if pressed && self.state.scope == Scope::Tasks {
            self.state.scope = Scope::Term;
          }
          if let Some(task) = self.state.current_task() {
            let event = mouse.translate(term_area);
            self.pc.send_msg(task.id, TaskScreenCmd::Mouse { event });
          }
        } else if layout.sidebar.contains(x, y) {
          if pressed && self.state.scope == Scope::Term {
            self.state.scope = Scope::Tasks;
          }
          let selected = self.state.selected();
          match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
              if let Some(index) = task_at(layout.sidebar, x, y, &self.state) {
                self.state.select(index);
              }
            }
            MouseEventKind::ScrollDown => {
              self.state.select(selected + 1);
            }
            MouseEventKind::ScrollUp => {
              self.state.select(selected.saturating_sub(1));
            }
            MouseEventKind::Down(MouseButton::Right | MouseButton::Middle)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Moved
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => (),
          }
        }
      }
      TermEvent::Resize(width, height) => {
        if let Some(client) =
          self.clients.iter_mut().find(|c| c.id == client_id)
        {
          client.size = Size { width, height };
        }
        self.fit_grid();
      }
      TermEvent::Paste(text) => {
        if self.modal.is_none()
          && self.state.scope.is_term()
          && let Some(task) = self.state.current_task()
        {
          self.pc.send_msg(task.id, ProcessPaste(text));
        }
      }
      TermEvent::FocusGained | TermEvent::FocusLost => (),
    }
  }

  fn handle_key(&mut self, client_id: ClientId, key: Key) {
    match key.kind {
      KeyEventKind::Press | KeyEventKind::Repeat => (),
      KeyEventKind::Release => return,
    }
    if let Some(modal) = &mut self.modal {
      match modal.handle_key(&key, client_id) {
        ModalResult::Keep => (),
        ModalResult::Close => self.modal = None,
        ModalResult::Run(action) => {
          self.modal = None;
          self.handle_action(action);
        }
      }
      return;
    }
    let key = Key::new(key.code, key.mods);
    let group = self.state.keymap_group();
    if let Some(action) = self.keymap.action(group, &key) {
      self.handle_action(action.clone());
    } else if self.state.scope.is_term() {
      self.handle_action(Action::SendKey { key });
    }
  }

  fn handle_action(&mut self, action: Action) {
    let current = self.state.current_task().map(|t| t.id);
    match action {
      Action::Batch { cmds } => {
        for cmd in cmds {
          self.handle_action(cmd);
        }
      }

      Action::QuitOrAsk => self.modal = Some(Box::new(QuitModal)),
      Action::Quit => {
        self.state.quitting = true;
        self.issue(Command::Quit);
      }
      Action::ForceQuit => {
        self.state.quitting = true;
        self.issue(Command::Batch {
          commands: vec![
            Command::Kill {
              target: Target::All {
                all: TaskSpaceId::default_space(),
              },
            },
            Command::Quit,
          ],
        });
      }
      Action::Detach { client_id } => self.remove_client(client_id),

      Action::ToggleFocus => self.state.scope = self.state.scope.toggle(),
      Action::FocusTasks => self.state.scope = Scope::Tasks,
      Action::FocusTerm => self.state.scope = Scope::Term,
      Action::Zoom => self.state.scope = Scope::TermZoom,

      Action::ShowCommandsMenu => {
        self.modal = Some(Box::new(CommandsMenuModal::new()));
      }
      Action::CloseCurrentModal => self.modal = None,
      Action::ToggleKeymapWindow => {
        self.state.hide_keymap_window = !self.state.hide_keymap_window;
      }

      Action::NextTask => {
        let count = self.state.tasks.len();
        if count > 0 {
          self.state.select((self.state.selected() + 1) % count);
        }
      }
      Action::PrevTask => {
        let count = self.state.tasks.len();
        if count > 0 {
          self
            .state
            .select((self.state.selected() + count - 1) % count);
        }
      }
      Action::SelectTask { index } => self.state.select(index),

      Action::StartTask => {
        self.issue_current(current, |target| Command::Start { target })
      }
      Action::StopTask => {
        self.issue_current(current, |target| Command::Stop { target })
      }
      Action::KillTask => {
        self.issue_current(current, |target| Command::Kill { target })
      }
      Action::VetoTask => {
        self.issue_current(current, |target| Command::Veto { target })
      }
      Action::RestartTask => {
        self.issue_current(current, |target| Command::Restart { target })
      }
      Action::ForceRestartTask => {
        self.issue_current(current, |target| Command::ForceRestart { target })
      }
      Action::RestartAll => {
        self.issue_all(|target| Command::Restart { target })
      }
      Action::ForceRestartAll => {
        self.issue_all(|target| Command::ForceRestart { target })
      }

      Action::ShowAddTask => {
        self.modal = Some(Box::new(AddTaskModal::default()))
      }
      Action::AddTask { cmd, name } => {
        let name = name.unwrap_or_else(|| cmd.clone());
        let task = TaskConfig {
          path: self.unique_name(&name, None),
          cmd: Some(CmdConfig::Shell { shell: cmd }),
          ..TaskConfig::default()
        };
        spawn_config_task(&self.config, &self.pc, task, Vec::new(), true);
      }
      Action::DuplicateTask => {
        if let Some(task) = self.state.current_task() {
          let name = self.unique_name(&task.name(), None);
          self.pc.send_msg(task.id, DuplicateTask(Some(name)));
        }
      }
      Action::ShowRenameTask => {
        self.modal = Some(Box::new(RenameTaskModal::default()));
      }
      Action::RenameTask { name } => {
        if let Some(id) = current {
          let name = self.unique_name(&name, Some(id));
          self.pc.set_task_label(id, Some(name));
        }
      }
      Action::ShowRemoveTask => {
        if let Some(task) = self.state.current_task()
          && !task.is_up()
        {
          self.modal = Some(Box::new(RemoveTaskModal { id: task.id }));
        }
      }
      Action::RemoveTask { id } => self.pc.send(KernelCommand::RemoveTask(id)),

      Action::ScrollUp { n, unit } => {
        self.send_current(
          current,
          TaskScreenCmd::Scroll {
            delta: n.min(i32::MAX as usize) as i32,
            unit: kernel_scroll_unit(unit),
          },
        );
      }
      Action::ScrollDown { n, unit } => {
        self.send_current(
          current,
          TaskScreenCmd::Scroll {
            delta: -(n.min(i32::MAX as usize) as i32),
            unit: kernel_scroll_unit(unit),
          },
        );
      }
      Action::CopyModeEnter => {
        if current.is_some() {
          self.state.scope = Scope::Term;
        }
        self.send_current(current, TaskScreenCmd::CopyEnter);
      }
      Action::CopyModeLeave => {
        self.send_current(current, TaskScreenCmd::CopyLeave)
      }
      Action::CopyModeMove { dir } => self.send_current(
        current,
        TaskScreenCmd::CopyMove {
          dir: kernel_copy_move(dir),
        },
      ),
      Action::CopyModeEnd => {
        self.send_current(current, TaskScreenCmd::CopyBeginSelection)
      }
      Action::CopyModeCopy => {
        self.send_current(current, TaskScreenCmd::CopyYank)
      }
      Action::SendKey { key } => self.send_current(current, ProcessInput(key)),
    }
  }

  fn unique_name(&self, base: &str, exclude: Option<TaskId>) -> String {
    let names: Vec<_> =
      self.state.tasks.iter().map(|t| (t.id, t.name())).collect();
    unique_task_name(
      base,
      exclude,
      names.iter().map(|(id, name)| (*id, name.as_str())),
    )
  }

  fn send_current<T: Send + 'static>(&self, current: Option<TaskId>, msg: T) {
    if let Some(id) = current {
      self.pc.send_msg(id, msg);
    }
  }

  fn issue(&self, command: Command) {
    if let Err(err) = issue(&self.pc, &command) {
      log::error!("Command failed: {err}");
    }
  }

  fn issue_current(
    &self,
    current: Option<TaskId>,
    make: fn(Target) -> Command,
  ) {
    if let Some(id) = current {
      self.issue(make(Target::id(id)));
    }
  }

  fn issue_all(&self, make: fn(Target) -> Command) {
    let commands = self
      .state
      .tasks
      .iter()
      .map(|t| make(Target::id(t.id)))
      .collect();
    self.issue(Command::Batch { commands });
  }

  fn handle_screen_notify(&mut self, notify: FramedScreenNotify) {
    match notify {
      FramedScreenNotify::ObserveStarted { .. }
      | FramedScreenNotify::Render { .. }
      | FramedScreenNotify::Bell { .. } => (),
      FramedScreenNotify::CopyPresent { task_id, vt } => {
        if let Some(task) = self.state.task_mut(task_id) {
          task.present = vt;
        }
      }
      FramedScreenNotify::Yank { text } => crate::clipboard::copy(&text),
    }
  }

  fn handle_task_notify(&mut self, id: TaskId, notify: TaskNotify) {
    match notify {
      TaskNotify::Added {
        path,
        label,
        state,
        vt,
      } => self.add_task(id, label, path, state, vt),
      TaskNotify::StateChanged(state) => {
        if let Some(task) = self.state.task_mut(id) {
          task.status = state;
        }
      }
      TaskNotify::Removed => {
        let index = self.state.tasks.iter().position(|t| t.id == id);
        self.state.tasks.retain(|t| t.id != id);
        let selected = self.state.selected();
        match index {
          Some(index) if index < selected => self.state.select(selected - 1),
          _ => self.state.select(selected),
        }
      }
      TaskNotify::PathChanged(_, path) => {
        if let Some(task) = self.state.task_mut(id) {
          task.path = path;
        }
      }
      TaskNotify::LabelChanged(label) => {
        if let Some(task) = self.state.task_mut(id) {
          task.label = label;
        }
      }
    }
  }
}
