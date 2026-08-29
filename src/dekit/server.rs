use std::{path::PathBuf, sync::Arc};

use serde_json::Value;
use tokio::task::JoinSet;

use crate::{
  command::{CommandError, CommandResult, execute},
  config::{
    config::Config,
    hook::{Hook, watch_idle},
  },
  console::app::console_task_registration,
  daemon::{lockfile, socket::bind_server_socket},
  dekit::attach::attach_session,
  kernel::{
    kernel::Kernel,
    kernel_message::{
      KernelCommand, KernelQuery, KernelQueryResponse, RegisterError, SharedVt,
      TaskContext, TaskInfo, TaskSelector,
    },
    task::{TaskDef, TaskState},
    task_key::TaskSpaceId,
    task_path::TaskPath,
  },
  protocol::{
    ActResult, ConnReceiver, ConnSender, CtlMsg, RpcError, RpcRequest,
    RpcState, RpcTaskInfo, RpcWhy, RpcWhyDep, ScreenResult, TaskListResult,
    codes, ok_result, server_handshake,
  },
  target::Target,
  task::config_tasks::register_config_tasks,
  term::Size,
};

pub async fn run_server(
  working_dir: PathBuf,
  log_level: Option<&str>,
) -> anyhow::Result<()> {
  let (config, load_err) = match Config::load_dir(&working_dir) {
    Ok(config) => (config, None),
    Err(err) => (Config::make_default(), Some(err)),
  };
  let keymap = config.keymap.build();
  let config = Arc::new(config);

  let _logger = crate::logging::init(crate::logging::Config {
    binary: "dekit",
    cli_level: log_level,
    log_env: "DEKIT_LOG",
    file_env: "DEKIT_LOG_FILE",
    config_level: config.log.level.as_deref(),
    config_file: config.log.file.as_deref(),
    default_dir: Some(&working_dir),
  })?;

  if let Some(err) = load_err {
    log::warn!("Failed to load dekit config: {}", err);
  }

  // Create lock file and acquire exclusive flock.
  let lock_guard = lockfile::create_lock_file(&working_dir)?;
  log::info!("Lock file created for directory: {}", working_dir.display());

  #[cfg(unix)]
  crate::process::unix_processes_waiter::UnixProcessesWaiter::init()?;
  let mut kernel = Kernel::new();
  let pc = kernel.context();
  let socket_path = lock_guard.socket_path().to_path_buf();
  let console_id = pc.alloc_id();
  let console = console_task_registration(
    console_id,
    TaskDef {
      space: TaskSpaceId::dekit(),
      path: Some(TaskPath::new("console").expect("valid console path")),
      pinned: true,
      ..TaskDef::default()
    },
    config.clone(),
    keymap,
  );
  if let Err(err) = kernel.register_task_registration(console) {
    #[cfg(unix)]
    crate::process::unix_processes_waiter::UnixProcessesWaiter::uninit()?;
    anyhow::bail!("Failed to register console task: {err}")
  }
  let console = pc.get_task_sender(console_id);
  let kernel_handle = tokio::spawn(kernel.run());

  // Watch before any task exists so the first start→exit fires the hook.
  if let Some(hook) = config.on_idle.clone() {
    watch_idle(&pc, &config, TaskSelector::all(), hook, console);
  }

  let bootstrap = async {
    register_config_tasks(&config, &pc).await?;
    if let Some(hook) = &config.on_init {
      let Hook::Command(command) = hook else {
        anyhow::bail!("dekit on_init hook is not a command")
      };
      execute(&pc, &config, command).await?;
    }
    let socket = bind_server_socket(&socket_path).await?;
    log::info!("Server is listening.");
    anyhow::Ok(socket)
  }
  .await;
  let mut server_socket = match bootstrap {
    Ok(socket) => socket,
    Err(err) => {
      pc.send(KernelCommand::Quit);
      let _ = kernel_handle.await;
      #[cfg(unix)]
      crate::process::unix_processes_waiter::UnixProcessesWaiter::uninit()?;
      return Err(err);
    }
  };

  tokio::spawn(async move {
    log::debug!("Waiting for clients...");
    loop {
      match server_socket.accept().await {
        Ok((sender, receiver)) => {
          let pc = pc.clone();
          let config = config.clone();
          tokio::spawn(async move {
            dispatch_connection(pc, config, sender, receiver).await;
          });
        }
        Err(err) => {
          log::debug!("Server socket accept error: {}", err);
          break;
        }
      }
    }
  });

  kernel_handle.await?;

  // lock_guard is dropped here, removing lock + socket files.
  drop(lock_guard);

  #[cfg(unix)]
  crate::process::unix_processes_waiter::UnixProcessesWaiter::uninit()?;

  Ok(())
}

/// Serves an accepted connection: handshake, then any number of
/// concurrent requests answered as they finish, until the client hangs
/// up or an `attach` takes the connection over.
pub async fn dispatch_connection(
  pc: TaskContext,
  config: Arc<Config>,
  mut sender: ConnSender,
  mut receiver: ConnReceiver,
) {
  if let Err(err) = server_handshake(&mut sender, &mut receiver).await {
    log::debug!("Client handshake failed: {err}");
    return;
  }

  let mut replies: JoinSet<CtlMsg> = JoinSet::new();
  loop {
    tokio::select! {
      msg = receiver.recv_ctl() => {
        let request = match msg {
          Ok(CtlMsg::Request(request)) => request,
          Ok(msg) => {
            log::debug!("Ignoring client message {msg:?}");
            continue;
          }
          Err(err) => {
            log::debug!("Client connection closed: {err}");
            break;
          }
        };
        match RpcRequest::from_wire(&request.method, request.params) {
          Ok(RpcRequest::Attach { target, width, height }) => {
            // Earlier requests are answered before the screen stream starts.
            if flush(&mut replies, &mut sender).await.is_err() {
              return;
            }
            attach_session(
              &pc,
              request.id,
              target,
              Size { width, height },
              sender,
              receiver,
            )
            .await;
            return;
          }
          Ok(req) => {
            let (pc, config, id) = (pc.clone(), config.clone(), request.id);
            replies.spawn(async move {
              match handle_rpc(&pc, &config, req).await {
                Ok(result) => CtlMsg::ok(id, result),
                Err(error) => CtlMsg::err(id, error),
              }
            });
          }
          Err(error) => {
            if sender.send_ctl(CtlMsg::err(request.id, error)).await.is_err() {
              return;
            }
          }
        }
      }
      Some(reply) = replies.join_next(), if !replies.is_empty() => {
        if let Ok(reply) = reply && sender.send_ctl(reply).await.is_err() {
          return;
        }
      }
    }
  }
  let _ = flush(&mut replies, &mut sender).await;
}

async fn flush(
  replies: &mut JoinSet<CtlMsg>,
  sender: &mut ConnSender,
) -> anyhow::Result<()> {
  while let Some(reply) = replies.join_next().await {
    if let Ok(reply) = reply {
      sender.send_ctl(reply).await?;
    }
  }
  Ok(())
}

fn task_state(state: TaskState) -> RpcState {
  let (token, info) = match state {
    TaskState::Idle => ("idle", None),
    TaskState::Starting => ("starting", None),
    TaskState::Running => ("running", None),
    TaskState::Ready => ("ready", None),
    TaskState::Stopping => ("stopping", None),
    TaskState::Backoff => ("backoff", None),
    TaskState::Done(info) => ("done", Some(info)),
    TaskState::Exited(info) => ("exited", Some(info)),
  };
  RpcState {
    state: token.to_string(),
    exit_code: info.and_then(|i| i.code),
    signal: info.and_then(|i| i.signal),
  }
}

fn bad_target(err: impl ToString) -> RpcError {
  RpcError::new(codes::BAD_TARGET, err.to_string())
}

async fn list(
  pc: &TaskContext,
  target: &Target,
) -> Result<Vec<TaskInfo>, RpcError> {
  let selector = target.selector().map_err(bad_target)?;
  match pc.query(KernelQuery::ListTasks(selector)).await {
    Ok(KernelQueryResponse::TaskList(tasks)) => Ok(tasks),
    Ok(KernelQueryResponse::Explain(_)) | Err(_) => {
      Err(RpcError::internal("unexpected query response"))
    }
  }
}

/// The single match a one-task request needs.
fn one<T>(matches: Vec<T>, target: &Target) -> Result<T, RpcError> {
  let mut matches = matches.into_iter();
  match (matches.next(), matches.next()) {
    (Some(task), None) => Ok(task),
    (None, _) => Err(RpcError::new(
      codes::NO_MATCH,
      format!("no task matches '{}'", target),
    )),
    (Some(_), Some(_)) => Err(RpcError::new(
      codes::AMBIGUOUS,
      format!("'{}' matches more than one task", target),
    )),
  }
}

/// Exactly one task must match, and it must have a screen.
pub async fn resolve_screen(
  pc: &TaskContext,
  target: &Target,
) -> Result<(TaskInfo, SharedVt), RpcError> {
  let task = one(list(pc, target).await?, target)?;
  match task.vt.clone() {
    Some(vt) => Ok((task, vt)),
    None => Err(RpcError::new(
      codes::NO_SCREEN,
      format!("'{}' has no screen", task.name()),
    )),
  }
}

async fn handle_rpc(
  pc: &TaskContext,
  config: &Config,
  req: RpcRequest,
) -> Result<Value, RpcError> {
  match req {
    RpcRequest::Attach { .. } => Err(RpcError::internal(
      "attach is handled by the connection loop",
    )),

    RpcRequest::Command(command) => {
      let result = execute(pc, config, &command).await.map_err(|err| {
        let code = match &err {
          CommandError::InvalidTarget(_) => codes::BAD_TARGET,
          CommandError::InvalidCommand(_) => codes::INVALID_PARAMS,
          CommandError::Register(RegisterError::MissingDep(_)) => {
            codes::NO_MATCH
          }
          CommandError::Register(RegisterError::PathTaken(_)) => {
            codes::PATH_TAKEN
          }
          CommandError::Register(RegisterError::ReservedSpace(_)) => {
            codes::BAD_TARGET
          }
          CommandError::Register(RegisterError::IdTaken)
          | CommandError::KernelClosed => codes::INTERNAL,
        };
        RpcError::new(code, err.to_string())
      })?;
      match result {
        CommandResult::Matched(matched) => {
          serde_json::to_value(ActResult { matched })
            .map_err(RpcError::internal)
        }
        CommandResult::None => Ok(ok_result()),
      }
    }

    RpcRequest::Ls { target } => {
      let target = target.unwrap_or_else(|| Target::glob("**"));
      let tasks = list(pc, &target)
        .await?
        .into_iter()
        .map(|t| RpcTaskInfo {
          id: t.id,
          path: t.name(),
          label: t.label,
          state: task_state(t.state),
        })
        .collect();
      serde_json::to_value(TaskListResult { tasks }).map_err(RpcError::internal)
    }

    RpcRequest::Why { target } => {
      let selector = target.selector().map_err(bad_target)?;
      let explains = match pc.query(KernelQuery::Explain(selector)).await {
        Ok(KernelQueryResponse::Explain(explains)) => explains,
        Ok(KernelQueryResponse::TaskList(_)) | Err(_) => {
          return Err(RpcError::internal("unexpected query response"));
        }
      };
      let explain = one(explains, &target)?;
      let why = RpcWhy {
        id: explain.id,
        path: explain.name,
        state: task_state(explain.state),
        wanted: explain.wanted,
        supported: explain.supported,
        vetoed: explain.vetoed,
        pinned: explain.pinned,
        required_by: explain.required_by,
        deps: explain
          .deps
          .into_iter()
          .map(|d| RpcWhyDep {
            path: d.name,
            state: task_state(d.state),
            wanted: d.wanted,
            satisfied: d.satisfied,
          })
          .collect(),
        attempts: explain.attempts,
      };
      serde_json::to_value(why).map_err(RpcError::internal)
    }

    RpcRequest::Screen { target } => {
      let (_, vt) = resolve_screen(pc, &target).await?;
      let screen = vt
        .read()
        .map(|screen| crate::term::ansi::render_screen_ansi(&screen))
        .map_err(|_| RpcError::internal("screen lock poisoned"))?;
      serde_json::to_value(ScreenResult { screen }).map_err(RpcError::internal)
    }
  }
}

#[cfg(test)]
mod tests {
  use std::{sync::Arc, time::Duration};

  use tokio::{io::duplex, time::timeout};

  use super::*;
  use crate::{
    kernel::kernel::Kernel,
    protocol::{Request, client_handshake},
  };

  #[tokio::test]
  async fn answers_concurrent_requests_by_id() {
    let config = Arc::new(Config::make_default());
    let kernel = Kernel::new();
    let pc = kernel.context();
    let kernel_handle = tokio::spawn(kernel.run());

    let (client, server) = duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server);
    let connection = tokio::spawn(dispatch_connection(
      pc.clone(),
      config,
      ConnSender::new(server_write),
      ConnReceiver::new(server_read),
    ));
    let mut sender = ConnSender::new(client_write);
    let mut receiver = ConnReceiver::new(client_read);
    let hello = client_handshake(&mut sender, &mut receiver).await.unwrap();
    assert_eq!(hello.version, env!("CARGO_PKG_VERSION"));

    let requests = [
      (7, RpcRequest::Ls { target: None }),
      (
        8,
        RpcRequest::Why {
          target: Target::glob("nope"),
        },
      ),
      (9, RpcRequest::Ls { target: None }),
    ];
    for (id, request) in requests {
      let (method, params) = request.to_wire();
      sender
        .send_ctl(CtlMsg::Request(Request { id, method, params }))
        .await
        .unwrap();
    }
    let mut seen = Vec::new();
    for _ in 0..3 {
      match timeout(Duration::from_secs(2), receiver.recv_ctl())
        .await
        .unwrap()
        .unwrap()
      {
        CtlMsg::Response(response) => {
          match response.id {
            8 => assert_eq!(response.error.unwrap().code, codes::NO_MATCH),
            _ => assert!(response.error.is_none()),
          }
          seen.push(response.id);
        }
        msg => panic!("unexpected {msg:?}"),
      }
    }
    seen.sort();
    assert_eq!(seen, vec![7, 8, 9]);

    drop(sender);
    drop(receiver);
    timeout(Duration::from_secs(2), connection)
      .await
      .unwrap()
      .unwrap();
    pc.send(KernelCommand::Quit);
    timeout(Duration::from_secs(2), kernel_handle)
      .await
      .unwrap()
      .unwrap();
  }
}
