use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::{
  dekit::server::resolve_screen,
  kernel::{
    copy_mode::CopyMove,
    kernel_message::{SharedVt, TaskContext},
    task::TaskId,
    task_screen::{ObserverId, ScreenNotify, ScrollUnit, TaskScreenCmd},
  },
  protocol::{
    Bye, ConnReceiver, ConnSender, CtlMsg, Msg, ScreenCommand, codes,
    ctl::{EVENT_INPUT, EVENT_SCREEN},
    ok_result, screen,
  },
  target::Target,
  term::{ScreenDiffer, Size, TermEvent, Winsize, vt::emit},
};

pub async fn attach_session(
  pc: &TaskContext,
  request_id: u64,
  target: Target,
  size: Size,
  mut sender: ConnSender,
  mut receiver: ConnReceiver,
) {
  let (task, vt) = match resolve_screen(pc, &target).await {
    Ok(found) => found,
    Err(error) => {
      let _ = sender.send_ctl(CtlMsg::err(request_id, error)).await;
      return;
    }
  };
  let observer = ObserverId::new();
  let (sink, notifies) = unbounded_channel();
  pc.send_msg(
    task.id,
    TaskScreenCmd::Attach {
      observer,
      size: Winsize {
        x: size.width,
        y: size.height,
        x_px: 0,
        y_px: 0,
      },
      sink,
    },
  );
  let confirmed =
    match sender.send_ctl(CtlMsg::ok(request_id, ok_result())).await {
      Ok(()) => true,
      Err(err) => {
        log::warn!("attach: failed to confirm: {err}");
        false
      }
    };
  if confirmed {
    session(
      pc,
      task.id,
      observer,
      vt,
      notifies,
      &mut sender,
      &mut receiver,
    )
    .await;
  }
  // Whatever ended the session, the screen must not keep our geometry.
  pc.send_msg(task.id, TaskScreenCmd::Detach { observer });
  if confirmed {
    let _ = sender
      .send_ctl(CtlMsg::Bye(Bye {
        code: codes::QUIT.to_string(),
        message: String::new(),
      }))
      .await;
  }
}

async fn session(
  pc: &TaskContext,
  task: TaskId,
  observer: ObserverId,
  vt: SharedVt,
  mut notifies: UnboundedReceiver<ScreenNotify>,
  sender: &mut ConnSender,
  receiver: &mut ConnReceiver,
) {
  let mut differ = ScreenDiffer::new();
  // Copy-mode surface, painted instead of `vt` while set.
  let mut present: Option<SharedVt> = None;
  let mut title = String::new();
  let mut batch = Vec::new();
  loop {
    tokio::select! {
      n = notifies.recv_many(&mut batch, 256) => {
        if n == 0 {
          return;
        }
        let mut paint = false;
        let mut out = Vec::new();
        for notify in batch.drain(..) {
          match notify {
            ScreenNotify::Attached | ScreenNotify::Render => paint = true,
            ScreenNotify::Bell => out.push(0x07),
            ScreenNotify::CopyPresent { vt, .. } => {
              present = vt;
              paint = true;
            }
            ScreenNotify::Yank { text } => {
              emit::osc52_copy(&mut out, &text);
              // For terminals without OSC 52, while the runner is local.
              tokio::task::spawn_blocking(move || crate::clipboard::copy(&text));
            }
          }
        }
        if paint {
          if let Ok(screen) = vt.read()
            && screen.title() != title
          {
            title = screen.title().to_string();
            emit::osc_title(&mut out, &title);
          }
          if let Ok(screen) = present.as_ref().unwrap_or(&vt).read() {
            differ.diff(&mut out, &*screen);
          }
        }
        if !out.is_empty() && sender.send_out(out.into()).await.is_err() {
          return;
        }
      }
      msg = receiver.recv() => match msg {
        Some(Ok(Msg::Ctl(CtlMsg::Event(event)))) if event.name == EVENT_INPUT => {
          match serde_json::from_value::<TermEvent>(event.params) {
            Ok(event) => {
              pc.send_msg(task, TaskScreenCmd::Input { observer, event });
            }
            Err(err) => log::debug!("attach: dropping input event: {err}"),
          }
        }
        Some(Ok(Msg::Ctl(CtlMsg::Event(event)))) if event.name == EVENT_SCREEN => {
          match serde_json::from_value::<ScreenCommand>(event.params) {
            Ok(command) => pc.send_msg(task, screen_cmd(command)),
            Err(err) => log::debug!("attach: dropping screen event: {err}"),
          }
        }
        Some(Ok(msg)) => log::debug!("attach: ignoring {msg:?}"),
        Some(Err(err)) => {
          log::debug!("attach: closing: {err}");
          return;
        }
        None => return,
      },
    }
  }
}

fn screen_cmd(command: ScreenCommand) -> TaskScreenCmd {
  match command {
    ScreenCommand::Scroll { delta, unit } => TaskScreenCmd::Scroll {
      delta,
      unit: match unit {
        screen::ScrollUnit::Line => ScrollUnit::Line,
        screen::ScrollUnit::HalfScreen => ScrollUnit::HalfScreen,
        screen::ScrollUnit::Screen => ScrollUnit::Screen,
      },
    },
    ScreenCommand::CopyEnter => TaskScreenCmd::CopyEnter,
    ScreenCommand::CopyLeave => TaskScreenCmd::CopyLeave,
    ScreenCommand::CopyMove { dir } => TaskScreenCmd::CopyMove {
      dir: match dir {
        screen::CopyMove::Up => CopyMove::Up,
        screen::CopyMove::Down => CopyMove::Down,
        screen::CopyMove::Left => CopyMove::Left,
        screen::CopyMove::Right => CopyMove::Right,
      },
    },
    ScreenCommand::CopySelect => TaskScreenCmd::CopyBeginSelection,
    ScreenCommand::CopyYank => TaskScreenCmd::CopyYank,
  }
}

#[cfg(test)]
mod tests {
  use std::{sync::Arc, time::Duration};

  use tokio::{io::duplex, time::timeout};

  use crate::{
    config::config::Config,
    console::app::console_task_registration,
    dekit::server::dispatch_connection,
    kernel::{
      kernel::Kernel,
      kernel_message::{KernelCommand, TaskContext},
      task::TaskDef,
      task_key::{TaskKey, TaskSpaceId},
      task_path::TaskPath,
    },
    protocol::{
      ConnReceiver, ConnSender, CtlMsg, Event, Msg, Request, RpcRequest,
      ScreenCommand, client_handshake,
      ctl::{EVENT_INPUT, EVENT_SCREEN},
      screen,
    },
    term::{
      TermEvent,
      key::{Key, KeyCode, KeyMods},
    },
  };

  /// The next `Out` frame, or None if the session ended first.
  async fn next_out(receiver: &mut ConnReceiver) -> Option<Vec<u8>> {
    loop {
      match receiver.recv().await {
        Some(Ok(Msg::Out(bytes))) => return Some(bytes.to_vec()),
        Some(Ok(Msg::Ctl(CtlMsg::Response(response)))) => {
          assert!(response.error.is_none(), "{:?}", response.error);
        }
        Some(Ok(Msg::Ctl(_))) => (),
        Some(Err(_)) | None => return None,
      }
    }
  }

  /// Reads `Out` frames until one contains `needle`.
  async fn wait_for(receiver: &mut ConnReceiver, needle: &[u8]) -> bool {
    timeout(Duration::from_secs(2), async {
      let mut out = Vec::new();
      while let Some(frame) = next_out(receiver).await {
        out.extend(frame);
        if out.windows(needle.len()).any(|w| w == needle) {
          return true;
        }
      }
      false
    })
    .await
    .unwrap()
  }

  /// A client attached to `target` through a served connection.
  async fn attach(
    pc: &TaskContext,
    config: &Arc<Config>,
    target: &str,
  ) -> (ConnSender, ConnReceiver, tokio::task::JoinHandle<()>) {
    let (client, server) = duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server);
    let session = tokio::spawn(dispatch_connection(
      pc.clone(),
      config.clone(),
      ConnSender::new(server_write),
      ConnReceiver::new(server_read),
    ));
    let mut sender = ConnSender::new(client_write);
    let mut receiver = ConnReceiver::new(client_read);
    client_handshake(&mut sender, &mut receiver).await.unwrap();
    let (method, params) = RpcRequest::Attach {
      target: target.parse().unwrap(),
      width: 80,
      height: 24,
    }
    .to_wire();
    sender
      .send_ctl(CtlMsg::Request(Request {
        id: 1,
        method,
        params,
      }))
      .await
      .unwrap();
    (sender, receiver, session)
  }

  async fn finish(
    pc: TaskContext,
    sender: ConnSender,
    receiver: ConnReceiver,
    session: tokio::task::JoinHandle<()>,
    kernel: tokio::task::JoinHandle<()>,
  ) {
    // Dropping the client ends the session.
    drop(sender);
    drop(receiver);
    timeout(Duration::from_secs(2), session)
      .await
      .unwrap()
      .unwrap();
    pc.send(KernelCommand::Quit);
    timeout(Duration::from_secs(2), kernel)
      .await
      .unwrap()
      .unwrap();
  }

  #[tokio::test]
  async fn attaches_to_the_console_and_forwards_input() {
    let config = Arc::new(Config::make_default());
    let keymap = config.keymap.build();
    let mut kernel = Kernel::new();
    let pc = kernel.context();
    let console_id = pc.alloc_id();
    kernel
      .register_task_registration(console_task_registration(
        console_id,
        TaskDef {
          space: TaskSpaceId::dekit(),
          path: Some(TaskPath::new("console").unwrap()),
          ..TaskDef::default()
        },
        config.clone(),
        keymap,
      ))
      .unwrap();
    let kernel_handle = tokio::spawn(kernel.run());

    let (mut sender, mut receiver, session) =
      attach(&pc, &config, "@dekit/console").await;

    // The console paints its sidebar once attached.
    assert!(wait_for(&mut receiver, b"Tasks").await);

    // Input reaches the console: `?` toggles the help window.
    let key = Key::new(KeyCode::Char('?'), KeyMods::NONE);
    sender
      .send_ctl(CtlMsg::Event(Event {
        name: EVENT_INPUT.to_string(),
        params: serde_json::to_value(TermEvent::Key(key)).unwrap(),
      }))
      .await
      .unwrap();
    let repainted = timeout(Duration::from_secs(2), next_out(&mut receiver))
      .await
      .unwrap();
    assert!(repainted.is_some());

    finish(pc, sender, receiver, session, kernel_handle).await;
  }

  #[tokio::test]
  async fn console_quit_key_detaches_without_stopping_runner() {
    let config = Arc::new(Config::make_default());
    let keymap = config.keymap.build();
    let mut kernel = Kernel::new();
    let pc = kernel.context();
    let console_id = pc.alloc_id();
    kernel
      .register_task_registration(console_task_registration(
        console_id,
        TaskDef {
          space: TaskSpaceId::dekit(),
          path: Some(TaskPath::new("console").unwrap()),
          ..TaskDef::default()
        },
        config.clone(),
        keymap,
      ))
      .unwrap();
    let kernel_handle = tokio::spawn(kernel.run());

    let (mut sender, mut receiver, session) =
      attach(&pc, &config, "@dekit/console").await;
    assert!(wait_for(&mut receiver, b"Tasks").await);

    sender
      .send_ctl(CtlMsg::Event(Event {
        name: EVENT_INPUT.to_string(),
        params: serde_json::to_value(TermEvent::Key(Key::new(
          KeyCode::Char('q'),
          KeyMods::NONE,
        )))
        .unwrap(),
      }))
      .await
      .unwrap();

    // The attachment closes, but the runner remains available for another.
    assert!(
      timeout(Duration::from_secs(2), next_out(&mut receiver))
        .await
        .unwrap()
        .is_none()
    );
    timeout(Duration::from_secs(2), session)
      .await
      .unwrap()
      .unwrap();
    drop(sender);
    drop(receiver);

    let (sender, mut receiver, session) =
      attach(&pc, &config, "@dekit/console").await;
    assert!(wait_for(&mut receiver, b"Tasks").await);
    finish(pc, sender, receiver, session, kernel_handle).await;
  }

  #[cfg(not(windows))]
  #[tokio::test]
  async fn screen_commands_drive_copy_mode_on_a_process() {
    use crate::{
      process::process_spec::ProcessSpec,
      task::process_task::{ProcessTaskConfig, process_task_registration},
    };

    let config = Arc::new(Config::make_default());
    let mut kernel = Kernel::new();
    let pc = kernel.context();
    let spec = ProcessSpec::from_argv(vec![
      "sh".to_string(),
      "-c".to_string(),
      "echo hello-copy; sleep 30".to_string(),
    ]);
    kernel
      .register_task_registration(process_task_registration(
        pc.alloc_id(),
        Some(TaskKey::default_space(TaskPath::new("echo").unwrap())),
        ProcessTaskConfig {
          pinned: true,
          ..ProcessTaskConfig::new(spec)
        },
      ))
      .unwrap();
    let kernel_handle = tokio::spawn(kernel.run());

    let (mut sender, mut receiver, session) =
      attach(&pc, &config, "echo").await;
    assert!(wait_for(&mut receiver, b"hello-copy").await);

    // Select the first cell of the top row and yank it: OSC 52 comes back.
    for command in [
      ScreenCommand::CopyEnter,
      ScreenCommand::Scroll {
        delta: 1,
        unit: screen::ScrollUnit::Screen,
      },
      ScreenCommand::CopySelect,
      ScreenCommand::CopyMove {
        dir: screen::CopyMove::Right,
      },
      ScreenCommand::CopyYank,
    ] {
      sender
        .send_ctl(CtlMsg::Event(Event {
          name: EVENT_SCREEN.to_string(),
          params: serde_json::to_value(command).unwrap(),
        }))
        .await
        .unwrap();
    }
    assert!(wait_for(&mut receiver, b"\x1b]52;;").await);

    // No SIGCHLD waiter in unit tests: remove the task so quit can finish.
    pc.send(KernelCommand::Remove(
      crate::kernel::kernel_message::TaskSelector::all(),
      None,
    ));
    finish(pc, sender, receiver, session, kernel_handle).await;
  }
}
