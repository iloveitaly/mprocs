use tokio::sync::mpsc::UnboundedReceiver;

use crate::{
  command::{Command, Target, issue},
  config::{
    config::Config,
    hook::Hook,
    task::{CmdConfig, TaskConfig},
  },
  console::{
    action::{Action, CopyMove, ScrollUnit},
    app_client::ClientHandle,
    app_layout::AppLayout,
    keymap::Keymap,
    modal::{
      add_task::AddTaskModal, commands_menu::CommandsMenuModal, modal::Modal,
      quit::QuitModal, remove_task::RemoveTaskModal,
      rename_task::RenameTaskModal,
    },
    state::{Scope, State},
    task::view::TaskView,
    ui_keymap::render_keymap,
    ui_tasks::{render_tasks, tasks_check_hit, tasks_get_clicked_index},
    ui_term::{render_term, term_check_hit},
    ui_zoom_tip::render_zoom_tip,
    widgets::list::ListState,
  },
};
use crate::{
  console::server_message::{ClientId, ServerMessage},
  error::ResultLogger,
  kernel::{
    copy_mode::CopyMove as KernelCopyMove,
    kernel_message::{
      KernelCommand, KernelQuery, KernelQueryResponse, TaskContext,
      TaskSelector,
    },
    sub_trie::SubMode,
    task::{TaskCmd, TaskDef, TaskId, TaskNotification, TaskNotify},
    task_key::{TaskKey, TaskSpaceId},
    task_path::TaskPath,
    task_screen::{
      FramedScreenNotify, ScrollUnit as KernelScrollUnit, TaskScreenCmd,
    },
  },
  protocol::{Bye, CtlMsg, codes},
  task::{
    config_tasks::{
      register_config_tasks, spawn_config_task, unique_task_name,
    },
    process_task::{DuplicateTask, ProcessInput},
  },
  term::{
    Grid, Size, TermEvent, Winsize,
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

#[derive(Debug, Default, PartialEq)]
pub enum LoopAction {
  Render,
  #[default]
  Skip,
  ForceQuit,
}

impl LoopAction {
  pub fn render(&mut self) {
    match self {
      LoopAction::Render => (),
      LoopAction::Skip => *self = LoopAction::Render,
      LoopAction::ForceQuit => (),
    }
  }

  fn force_quit(&mut self) {
    *self = LoopAction::ForceQuit;
  }
}

pub struct App {
  config: Config,
  keymap: Keymap,
  state: State,
  grid: Grid,
  modal: Option<Box<dyn Modal>>,
  pr: tokio::sync::mpsc::UnboundedReceiver<TaskCmd>,
  pc: TaskContext,

  screen_size: Size,
  clients: Vec<ClientHandle>,
  bootstrap: bool,
}

impl App {
  pub async fn run(self) -> anyhow::Result<()> {
    let result = self.main_loop().await;
    if let Err(err) = result {
      log::error!("App main loop error: {err}");
    }

    Ok(())
  }

  async fn main_loop(mut self) -> anyhow::Result<()> {
    self.pc.subscribe_path(
      TaskKey::default_space(TaskPath::root()),
      SubMode::Subtree,
    );
    self.refresh_tasks().await;

    if self.bootstrap {
      register_config_tasks(&self.config, &self.pc).await?;
    }

    let mut render_needed = true;
    let mut last_term_size = self.get_layout().term_area().size();

    let mut command_buf = Vec::new();

    loop {
      let layout = self.get_layout();

      let term_size = layout.term_area().size();
      if term_size != last_term_size {
        let observer_id = self.pc.task_id;
        for task_handle in &mut self.state.tasks {
          self.pc.send_msg(
            task_handle.id(),
            TaskScreenCmd::Resize {
              size: Winsize {
                x: term_size.width,
                y: term_size.height,
                x_px: 0,
                y_px: 0,
              },
              observer_id,
            },
          );
        }

        last_term_size = term_size;
      }

      if render_needed && self.clients.len() > 0 {
        let grid = &mut self.grid;
        grid.erase_all(Attrs::default());
        grid.cursor_pos = None;
        grid.cursor_style = crate::term::CursorStyle::Default;

        let state = &mut self.state;
        let config = &mut self.config;
        let keymap = &self.keymap;
        render_tasks(layout.sidebar.into(), grid, state, config);
        render_term(layout.term, grid, state);
        render_keymap(layout.keymap.into(), grid, state, keymap);
        render_zoom_tip(layout.zoom_banner.into(), grid, keymap);

        if let Some(modal) = &mut self.modal {
          grid.cursor_style = crate::term::CursorStyle::Default;
          modal.render(grid);
        }

        for client_handle in &mut self.clients {
          let mut out = String::new();
          client_handle.differ.diff(&mut out, grid).log_ignore();
          client_handle
            .sender
            .send_out(out.into_bytes().into())
            .await
            .log_ignore();
        }
      }

      let mut loop_action = LoopAction::default();
      self.pr.recv_many(&mut command_buf, 512).await;
      for command in command_buf.drain(..) {
        self.handle_task_command(&mut loop_action, command);
      }

      if self.state.quitting && self.state.all_tasks_down() {
        break;
      }

      match loop_action {
        LoopAction::Render => {
          render_needed = true;
        }
        LoopAction::Skip => {
          render_needed = false;
        }
        LoopAction::ForceQuit => break,
      };
    }

    for mut client in self.clients.into_iter() {
      client
        .sender
        .send_ctl(CtlMsg::Bye(Bye {
          code: codes::QUIT.to_string(),
          message: String::new(),
        }))
        .await
        .log_ignore();
    }

    for task in &self.state.tasks {
      self.pc.send_msg(
        task.id(),
        TaskScreenCmd::Unobserve {
          observer_id: self.pc.task_id,
        },
      );
    }
    self.pc.unsubscribe_path(
      TaskKey::default_space(TaskPath::root()),
      SubMode::Subtree,
    );

    Ok(())
  }

  fn observe_task(&self, task_id: TaskId, size: Rect) {
    let sender = self.pc.get_task_sender(self.pc.task_id);
    self.pc.send_msg(
      task_id,
      TaskScreenCmd::Observe {
        size: Winsize {
          x: size.width,
          y: size.height,
          x_px: 0,
          y_px: 0,
        },
        sender,
      },
    );
  }

  async fn refresh_tasks(&mut self) {
    let resp = self
      .pc
      .query(KernelQuery::ListTasks(TaskSpaceId::default_space(), None))
      .await;
    let Ok(KernelQueryResponse::TaskList(list)) = resp else {
      return;
    };
    let size = self.get_layout().term_area();
    for task in list {
      let Some(vt) = task.vt else {
        continue;
      };
      if self.state.tasks.iter().any(|p| p.id() == task.id) {
        continue;
      }
      let name = task_display_name(task.label, task.path.as_ref(), task.id);
      self
        .state
        .tasks
        .push(TaskView::new(task.id, name, task.state, vt));
      self.observe_task(task.id, size);
    }
  }

  fn handle_server_message(
    &mut self,
    loop_action: &mut LoopAction,
    msg: ServerMessage,
  ) -> anyhow::Result<()> {
    match msg {
      ServerMessage::ClientInput { client_id, event } => {
        self.state.current_client_id = Some(client_id);
        self.handle_input(loop_action, client_id, event);
        self.state.current_client_id = None;
      }
      ServerMessage::ClientConnected { handle } => {
        self.clients.push(handle);
        self.update_screen_size();
        loop_action.render();
      }
      ServerMessage::ClientDisconnected { client_id } => {
        self.clients.retain(|c| c.id != client_id);
        self.update_screen_size();
        loop_action.render();
      }
    }
    Ok(())
  }

  fn update_screen_size(&mut self) {
    if let Some(client) = self.clients.first_mut() {
      self.screen_size = client.size();
      self.grid.set_size(client.size());
    }
  }

  fn handle_input(
    &mut self,
    loop_action: &mut LoopAction,
    client_id: ClientId,
    event: TermEvent,
  ) {
    if let TermEvent::Key(Key {
      kind: KeyEventKind::Release,
      ..
    }) = event
    {
      return;
    }

    if let Some(modal) = &mut self.modal {
      let handled = modal.handle_input(&mut self.state, loop_action, &event);
      if handled {
        return;
      }
    }

    match event {
      TermEvent::Key(Key {
        code,
        mods,
        kind: KeyEventKind::Press | KeyEventKind::Repeat,
        state: _,
      }) => {
        let key = Key::new(code, mods);
        let group = self.state.get_keymap_group();
        if let Some(bound) = self.keymap.resolve(group, &key) {
          let bound = bound.clone();
          self.handle_event(loop_action, &bound)
        } else {
          match self.state.scope {
            Scope::Tasks => (),
            Scope::Term | Scope::TermZoom => {
              self.handle_event(loop_action, &Action::SendKey { key })
            }
          }
        }
      }
      TermEvent::Key(Key {
        kind: KeyEventKind::Release,
        ..
      }) => (),
      TermEvent::Mouse(mouse_event) => {
        let layout = self.get_layout();
        if term_check_hit(
          layout.term_area(),
          mouse_event.x as u16,
          mouse_event.y as u16,
        ) {
          if let (Scope::Tasks, MouseEventKind::Down(_)) =
            (self.state.scope, mouse_event.kind)
          {
            self.state.scope = Scope::Term
          }
          if let Some(task) = self.state.get_current_task() {
            let local_event = mouse_event.translate(layout.term_area());
            self
              .pc
              .send_msg(task.id, TaskScreenCmd::Mouse { event: local_event });
          }
        } else if tasks_check_hit(
          layout.sidebar.into(),
          mouse_event.x as u16,
          mouse_event.y as u16,
        ) {
          if let (Scope::Term, MouseEventKind::Down(_)) =
            (self.state.scope, mouse_event.kind)
          {
            self.state.scope = Scope::Tasks
          }
          match mouse_event.kind {
            MouseEventKind::Down(btn) => match btn {
              MouseButton::Left => {
                if let Some(index) = tasks_get_clicked_index(
                  layout.sidebar.into(),
                  mouse_event.x as u16,
                  mouse_event.y as u16,
                  &self.state,
                ) {
                  self.state.select_task(index);
                }
              }
              MouseButton::Right | MouseButton::Middle => (),
            },
            MouseEventKind::Up(_) => (),
            MouseEventKind::Drag(_) => (),
            MouseEventKind::Moved => (),
            MouseEventKind::ScrollDown => {
              if self.state.selected()
                < self.state.tasks.len().saturating_sub(1)
              {
                let index = self.state.selected() + 1;
                self.state.select_task(index);
              }
            }
            MouseEventKind::ScrollUp => {
              if self.state.selected() > 0 {
                let index = self.state.selected() - 1;
                self.state.select_task(index);
              }
            }
            MouseEventKind::ScrollLeft => (),
            MouseEventKind::ScrollRight => (),
          }
        }
        loop_action.render();
      }
      TermEvent::Resize(width, height) => {
        if let Some(client) =
          self.clients.iter_mut().find(|c| c.id == client_id)
        {
          let size = Size { width, height };
          client.resize(size);
        }
        self.update_screen_size();

        loop_action.render();
      }
      TermEvent::FocusGained => {
        log::debug!("Ignore input event: {:?}", event);
      }
      TermEvent::FocusLost => {
        log::debug!("Ignore input event: {:?}", event);
      }
      TermEvent::Paste(_) => {
        log::debug!("Ignore input event: {:?}", event);
      }
    }
  }

  fn scroll(
    &self,
    loop_action: &mut LoopAction,
    delta: i32,
    unit: KernelScrollUnit,
  ) {
    if let Some(task) = self.state.get_current_task() {
      self
        .pc
        .send_msg(task.id, TaskScreenCmd::Scroll { delta, unit });
      loop_action.render();
    }
  }

  fn handle_event(&mut self, loop_action: &mut LoopAction, event: &Action) {
    let pc = self.pc.clone();
    match event {
      Action::Batch { cmds } => {
        for cmd in cmds {
          self.handle_event(loop_action, cmd);
          if *loop_action == LoopAction::ForceQuit {
            return;
          }
        }
      }

      Action::QuitOrAsk => {
        self.modal = Some(QuitModal::new(self.pc.clone()).boxed());
        loop_action.render();
      }
      Action::Quit => {
        self.state.quitting = true;
        self.issue_command(Command::Quit);
        loop_action.render();
      }
      Action::ForceQuit => {
        pc.send(KernelCommand::Quit);
        for task in self.state.tasks.iter() {
          if task.is_up() {
            pc.send(KernelCommand::Kill(TaskSelector::Id(task.id()), None));
          }
        }
        loop_action.force_quit();
      }
      Action::Detach { client_id } => {
        self.clients.retain_mut(|c| c.id != *client_id);
        self.update_screen_size();
        loop_action.render();
      }

      Action::ToggleFocus => {
        self.state.scope = self.state.scope.toggle();
        loop_action.render();
      }
      Action::FocusTasks => {
        self.state.scope = Scope::Tasks;
        loop_action.render();
      }
      Action::FocusTerm => {
        self.state.scope = Scope::Term;
        loop_action.render();
      }
      Action::Zoom => {
        self.state.scope = Scope::TermZoom;
        loop_action.render();
      }

      Action::ShowCommandsMenu => {
        self.modal =
          Some(CommandsMenuModal::new(self.pc.clone(), &self.keymap).boxed());
        loop_action.render();
      }
      Action::NextTask => {
        let mut next = self.state.selected() + 1;
        if next >= self.state.tasks.len() {
          next = 0;
        }
        self.state.select_task(next);
        loop_action.render();
      }
      Action::PrevTask => {
        let next = if self.state.selected() > 0 {
          self.state.selected() - 1
        } else {
          self.state.tasks.len().saturating_sub(1)
        };
        self.state.select_task(next);
        loop_action.render();
      }
      Action::SelectTask { index } => {
        self.state.select_task(*index);
        loop_action.render();
      }

      Action::StartTask => {
        if let Some(task) = self.state.get_current_task() {
          self.issue_command(Command::Start {
            target: Target::id(task.id),
          });
        }
      }
      Action::StopTask => {
        if let Some(task) = self.state.get_current_task() {
          self.issue_command(Command::Stop {
            target: Target::id(task.id),
          });
        }
      }
      Action::KillTask => {
        if let Some(task) = self.state.get_current_task() {
          self.issue_command(Command::Kill {
            target: Target::id(task.id),
          });
        }
      }
      Action::VetoTask => {
        if let Some(task) = self.state.get_current_task() {
          self.issue_command(Command::Veto {
            target: Target::id(task.id),
          });
        }
      }
      Action::RestartTask => {
        if let Some(task) = self.state.get_current_task() {
          self.issue_command(Command::Restart {
            target: Target::id(task.id),
          });
        }
      }
      Action::RestartAll => {
        self.issue_command(Command::Batch {
          commands: self
            .state
            .tasks
            .iter()
            .map(|task| Command::Restart {
              target: Target::id(task.id),
            })
            .collect(),
        });
      }
      Action::ForceRestartTask => {
        if let Some(task) = self.state.get_current_task() {
          self.issue_command(Command::ForceRestart {
            target: Target::id(task.id),
          });
        }
      }
      Action::ForceRestartAll => {
        self.issue_command(Command::Batch {
          commands: self
            .state
            .tasks
            .iter()
            .map(|task| Command::ForceRestart {
              target: Target::id(task.id),
            })
            .collect(),
        });
      }

      Action::ScrollUp { n, unit } => {
        let n = (*n).min(i32::MAX as usize) as i32;
        self.scroll(loop_action, n, kernel_scroll_unit(*unit));
      }
      Action::ScrollDown { n, unit } => {
        let n = (*n).min(i32::MAX as usize) as i32;
        self.scroll(loop_action, -n, kernel_scroll_unit(*unit));
      }
      Action::ShowAddTask => {
        self.modal = Some(AddTaskModal::new(self.pc.clone()).boxed());
        loop_action.render();
      }
      Action::AddTask { cmd, name } => {
        let name = name.clone().unwrap_or_else(|| cmd.clone());
        let task_config = TaskConfig {
          path: unique_task_name(
            &name,
            None,
            self.state.tasks.iter().map(|task| (task.id(), task.name())),
          ),
          cmd: Some(CmdConfig::Shell {
            shell: cmd.to_string(),
          }),
          ..TaskConfig::default()
        };
        let _ = spawn_config_task(
          &self.config,
          &self.pc,
          task_config,
          Vec::new(),
          true,
        );
        loop_action.render();
      }
      Action::DuplicateTask => {
        if let Some(task) = self.state.get_current_task() {
          let name = unique_task_name(
            task.name(),
            None,
            self.state.tasks.iter().map(|task| (task.id(), task.name())),
          );
          pc.send_msg(task.id(), DuplicateTask(Some(name)));
          loop_action.render();
        }
      }
      Action::ShowRemoveTask => {
        let id = match self.state.get_current_task() {
          Some(task) if !task.is_up() => Some(task.id()),
          _ => None,
        };
        if let Some(id) = id {
          self.modal = Some(RemoveTaskModal::new(id, self.pc.clone()).boxed());
          loop_action.render();
        }
      }
      Action::RemoveTask { id } => {
        self.pc.send(KernelCommand::RemoveTask(*id));
        loop_action.render();
      }

      Action::CloseCurrentModal => {
        self.modal = None;
        loop_action.render();
      }

      Action::ShowRenameTask => {
        self.modal = Some(RenameTaskModal::new(self.pc.clone()).boxed());
        loop_action.render();
      }
      Action::RenameTask { name } => {
        if let Some(task) = self.state.get_current_task() {
          let id = task.id();
          let name = unique_task_name(
            name,
            Some(id),
            self.state.tasks.iter().map(|task| (task.id(), task.name())),
          );
          self.pc.set_task_label(id, Some(name));
          loop_action.render();
        }
      }

      Action::CopyModeEnter => {
        if let Some(task) = self.state.get_current_task() {
          pc.send_msg(task.id, TaskScreenCmd::CopyEnter);
          self.state.scope = Scope::Term;
          loop_action.render();
        };
      }
      Action::CopyModeLeave => {
        if let Some(task) = self.state.get_current_task() {
          pc.send_msg(task.id, TaskScreenCmd::CopyLeave);
        }
      }
      Action::CopyModeMove { dir } => {
        if let Some(task) = self.state.get_current_task() {
          pc.send_msg(
            task.id,
            TaskScreenCmd::CopyMove {
              dir: kernel_copy_move(*dir),
            },
          );
        }
      }
      Action::CopyModeEnd => {
        if let Some(task) = self.state.get_current_task() {
          pc.send_msg(task.id, TaskScreenCmd::CopyBeginSelection);
        }
      }
      Action::CopyModeCopy => {
        if let Some(task) = self.state.get_current_task() {
          pc.send_msg(task.id, TaskScreenCmd::CopyYank);
        }
      }

      Action::ToggleKeymapWindow => {
        self.state.toggle_keymap_window();
        loop_action.render();
      }

      Action::SendKey { key } => {
        if let Some(task) = self.state.get_current_task() {
          pc.send_msg(task.id, ProcessInput(*key));
        }
      }
    }
  }

  fn issue_command(&self, command: Command) {
    if let Err(err) = issue(&self.pc, &command) {
      log::error!("Command failed: {err}");
    }
  }

  fn handle_task_command(
    &mut self,
    loop_action: &mut LoopAction,
    command: TaskCmd,
  ) {
    match command {
      TaskCmd::Start => (),
      TaskCmd::Stop => {
        self.state.quitting = true;
        loop_action.render();
      }
      TaskCmd::Kill => loop_action.force_quit(),

      TaskCmd::Msg(msg) => {
        let msg = match msg.downcast::<Action>() {
          Ok(app_event) => {
            self.handle_event(loop_action, &app_event);
            return;
          }
          Err(msg) => msg,
        };
        let msg = match msg.downcast::<ServerMessage>() {
          Ok(server_msg) => {
            let r = self.handle_server_message(loop_action, *server_msg);
            if let Err(err) = r {
              log::debug!("ServerMessage error: {:?}", err);
            }
            return;
          }
          Err(msg) => msg,
        };
        let msg = match msg.downcast::<FramedScreenNotify>() {
          Ok(notify) => {
            self.handle_screen_notify(loop_action, *notify);
            return;
          }
          Err(msg) => msg,
        };
        if let Ok(n) = msg.downcast::<TaskNotification>() {
          self.handle_notification(loop_action, n.from, n.notify);
          return;
        }
        log::error!("App received unknown Msg");
      }
    }
  }

  fn handle_screen_notify(
    &mut self,
    loop_action: &mut LoopAction,
    notify: FramedScreenNotify,
  ) {
    match notify {
      FramedScreenNotify::ObserveStarted { task_id } => {
        let is_current = self
          .state
          .get_current_task()
          .is_some_and(|p| p.id() == task_id);
        if is_current {
          loop_action.render();
        }
      }
      FramedScreenNotify::Render { task_id } => {
        let is_current = self
          .state
          .get_current_task()
          .is_some_and(|p| p.id() == task_id);
        if let Some(task) = self.state.get_task_mut(task_id) {
          if !is_current {
            task.changed = true;
          }
          loop_action.render();
        }
      }
      FramedScreenNotify::Bell { .. } => (),
      FramedScreenNotify::CopyPresent { task_id, vt } => {
        if let Some(task) = self.state.get_task_mut(task_id) {
          task.present = vt;
          loop_action.render();
        }
      }
      FramedScreenNotify::Yank { text } => {
        crate::clipboard::copy(text.as_str());
      }
    }
  }

  fn handle_notification(
    &mut self,
    loop_action: &mut LoopAction,
    task_id: TaskId,
    notify: TaskNotify,
  ) {
    match notify {
      TaskNotify::Added {
        path,
        label,
        state,
        vt,
      } => {
        let Some(vt) = vt else {
          return;
        };
        if self.state.tasks.iter().any(|p| p.id() == task_id) {
          return;
        }
        let name = task_display_name(label, path.as_ref(), task_id);
        self
          .state
          .tasks
          .push(TaskView::new(task_id, name, state, vt));
        let size = self.get_layout().term_area();
        self.observe_task(task_id, size);
        loop_action.render();
      }
      TaskNotify::StateChanged(state) => {
        let known = if let Some(task) = self.state.get_task_mut(task_id) {
          task.status = state;
          true
        } else {
          false
        };
        if known {
          if !state.is_active() && self.state.all_tasks_down() {
            if let Some(hook) = &self.config.on_all_finished {
              match hook.clone() {
                Hook::Command(command) => self.issue_command(command),
                Hook::LegacyAction(event) => {
                  self.handle_event(loop_action, &event);
                }
              }
            }
          }
          loop_action.render();
        }
      }
      TaskNotify::Removed => {
        self.state.tasks.retain(|p| p.id() != task_id);
        loop_action.render();
      }
      TaskNotify::PathChanged(_, new) => {
        if let Some(new) = new
          && let Some(task) = self.state.get_task_mut(task_id)
        {
          task.set_name(new.name().to_string());
        }
      }
      TaskNotify::LabelChanged(label) => {
        if let Some(task) = self.state.get_task_mut(task_id) {
          task.set_name(task_display_name(label, None, task_id));
          loop_action.render();
        }
      }
    }
  }

  fn get_layout(&mut self) -> AppLayout {
    let size = self.screen_size;
    AppLayout::new(
      Rect::new(0, 0, size.width, size.height),
      self.state.scope.is_zoomed(),
      self.state.hide_keymap_window,
      &self.config,
    )
  }
}

fn task_display_name(
  label: Option<String>,
  path: Option<&TaskPath>,
  id: TaskId,
) -> String {
  label
    .or_else(|| path.map(|p| p.name().to_string()))
    .unwrap_or_else(|| format!("task-{}", id.0))
}

pub fn create_app_task(
  config: Config,
  keymap: Keymap,
  pc: &TaskContext,
  bootstrap: bool,
) -> (TaskId, tokio::sync::oneshot::Receiver<bool>) {
  let task_id = pc.alloc_id();
  let ack = pc.register_task(app_task_registration(
    task_id,
    TaskDef::default(),
    config,
    keymap,
    bootstrap,
  ));
  (task_id, ack)
}

pub fn console_task_registration(
  task_id: TaskId,
  config: Config,
  keymap: Keymap,
) -> crate::kernel::kernel_message::TaskRegistration {
  app_task_registration(
    task_id,
    TaskDef {
      space: TaskSpaceId::dekit(),
      path: Some(TaskPath::new("console").expect("valid console path")),
      ..TaskDef::default()
    },
    config,
    keymap,
    false,
  )
}

fn app_task_registration(
  task_id: TaskId,
  def: TaskDef,
  config: Config,
  keymap: Keymap,
  bootstrap: bool,
) -> crate::kernel::kernel_message::TaskRegistration {
  crate::kernel::kernel_message::TaskRegistration::async_task(
    task_id,
    def,
    move |pc, receiver| async move {
      log::debug!("Creating app task (id: {})", pc.task_id.0);
      if let Err(err) =
        server_main(config, keymap, receiver, pc.clone(), bootstrap).await
      {
        log::error!("App task finished with error: {:?}", err);
      }
      pc.send(KernelCommand::Quit);
    },
  )
}

pub async fn server_main(
  config: Config,
  keymap: Keymap,
  pr: UnboundedReceiver<TaskCmd>,
  pc: TaskContext,
  bootstrap: bool,
) -> anyhow::Result<()> {
  let state = State {
    current_client_id: None,

    scope: Scope::Tasks,
    tasks: Vec::new(),
    tasks_list: ListState::default(),
    hide_keymap_window: !config.tui.tips.show,

    quitting: false,
  };

  let size = Size {
    width: 160,
    height: 50,
  };
  let scrollback_len = config.defaults.scrollback_len();

  let app = App {
    config,
    keymap,
    state,
    grid: Grid::new(size, scrollback_len),
    modal: None,
    pr,
    pc,

    screen_size: size,
    clients: Vec::new(),
    bootstrap,
  };

  if bootstrap
    && let Some(hook) = &app.config.on_init
    && let Hook::LegacyAction(action) = hook
  {
    app.pc.send_self_custom(action.clone());
  }

  app.run().await?;

  Ok(())
}
