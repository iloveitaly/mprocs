use std::path::PathBuf;

use serde_json::Value;

use crate::{
  command::{Command, CommandError, CommandResult, Target, execute},
  config::hook::{Hook, watch_idle},
  console::{
    app::console_task_registration,
    client::{ClientId, client_session},
  },
  daemon::{lockfile, socket::bind_server_socket},
  kernel::{
    kernel::Kernel,
    kernel_message::{
      KernelCommand, KernelQuery, KernelQueryResponse, TaskContext, TaskInfo,
      TaskSelector,
    },
    task::{TaskDef, TaskState},
    task_key::{TaskKey, TaskSpaceId},
    task_path::TaskPath,
  },
  protocol::{
    ActResult, ConnReceiver, ConnSender, CtlMsg, RpcError, RpcRequest,
    RpcState, RpcTaskInfo, RpcWhy, RpcWhyDep, SpawnResult, TaskListResult,
    codes, ok_result, server_handshake,
  },
  task::config_tasks::register_config_tasks,
  term::Size,
};

pub async fn run_server(
  working_dir: PathBuf,
  log_level: Option<&str>,
) -> anyhow::Result<()> {
  let (config, keymap, load_err) =
    match crate::config::config::Config::load_dir(&working_dir) {
      Ok(config) => {
        let keymap = config.keymap.build();
        (config, keymap, None)
      }
      Err(err) => {
        let config = crate::config::config::Config::make_default();
        let keymap = config.keymap.build();
        (config, keymap, Some(err))
      }
    };

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
  let on_init = config.on_init.clone();
  let app_task_id = pc.alloc_id();
  let (app_registration, console_done) = console_task_registration(
    app_task_id,
    TaskDef {
      space: TaskSpaceId::dekit(),
      path: Some(TaskPath::new("console").expect("valid console path")),
      ..TaskDef::default()
    },
    config.clone(),
    keymap,
  );
  if !kernel.register_task_registration(app_registration) {
    #[cfg(unix)]
    crate::process::unix_processes_waiter::UnixProcessesWaiter::uninit()?;
    anyhow::bail!("Failed to register console task.")
  }
  let app_sender = pc.get_task_sender(app_task_id);
  let kernel_handle = tokio::spawn(kernel.run());

  // Watch before any task exists so the first start→exit fires the hook.
  if let Some(hook) = config.on_idle.clone() {
    watch_idle(
      &pc,
      TaskSelector::All(TaskSpaceId::default_space()),
      hook,
      app_sender.clone(),
    );
  }

  if let Err(err) = register_config_tasks(&config, &pc).await {
    pc.send(KernelCommand::Quit);
    let _ = kernel_handle.await;
    #[cfg(unix)]
    crate::process::unix_processes_waiter::UnixProcessesWaiter::uninit()?;
    return Err(err);
  }

  let bootstrap = async {
    if let Some(hook) = on_init {
      let Hook::Command(command) = hook else {
        anyhow::bail!("dekit on_init hook is not a command")
      };
      execute(&pc, &command).await?;
    }
    anyhow::Ok(())
  }
  .await;
  if let Err(err) = bootstrap {
    pc.send(KernelCommand::Quit);
    let _ = kernel_handle.await;
    #[cfg(unix)]
    crate::process::unix_processes_waiter::UnixProcessesWaiter::uninit()?;
    return Err(err);
  }

  let mut server_socket = match bind_server_socket(&socket_path).await {
    Ok(server_socket) => {
      log::info!("Server is listening.");
      server_socket
    }
    Err(err) => {
      pc.send(KernelCommand::Quit);
      let _ = kernel_handle.await;
      #[cfg(unix)]
      crate::process::unix_processes_waiter::UnixProcessesWaiter::uninit()?;
      return Err(err.into());
    }
  };

  tokio::spawn(async move {
    let mut last_client_id = 0;
    log::debug!("Waiting for clients...");
    loop {
      match server_socket.accept().await {
        Ok((sender, receiver)) => {
          last_client_id += 1;
          let client_id = ClientId(last_client_id);
          let app_sender = app_sender.clone();
          let pc = pc.clone();
          tokio::spawn(async move {
            dispatch_connection(client_id, app_sender, pc, sender, receiver)
              .await;
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
  // Let the console say goodbye to attached clients, but never let a
  // stalled client keep the server (and the lock file) alive.
  let _ = tokio::time::timeout(
    std::time::Duration::from_secs(2),
    console_done,
  )
  .await;

  // lock_guard is dropped here, removing lock + socket files.
  drop(lock_guard);

  #[cfg(unix)]
  crate::process::unix_processes_waiter::UnixProcessesWaiter::uninit()?;

  Ok(())
}

/// Dispatch an accepted connection: handshake, then one RPC request or an
/// attach session.
pub async fn dispatch_connection(
  client_id: ClientId,
  app_sender: crate::kernel::kernel_message::TaskSender,
  pc: TaskContext,
  mut sender: ConnSender,
  mut receiver: ConnReceiver,
) {
  if let Err(err) = server_handshake(&mut sender, &mut receiver).await {
    log::debug!("Client handshake failed: {err}");
    return;
  }

  let request = match receiver.recv_ctl().await {
    Ok(CtlMsg::Request(request)) => request,
    Ok(msg) => {
      log::warn!("Expected a request from client, got {msg:?}");
      return;
    }
    Err(err) => {
      log::debug!("Client connection closed: {err}");
      return;
    }
  };

  match RpcRequest::from_wire(&request.method, request.params) {
    Ok(RpcRequest::TuiAttach { width, height }) => {
      client_session(
        client_id,
        app_sender,
        Size { width, height },
        request.id,
        sender,
        receiver,
      )
      .await;
    }
    Ok(req) => {
      let msg = match handle_rpc(&pc, req).await {
        Ok(result) => CtlMsg::ok(request.id, result),
        Err(error) => CtlMsg::err(request.id, error),
      };
      let _ = sender.send_ctl(msg).await;
    }
    Err(error) => {
      let _ = sender.send_ctl(CtlMsg::err(request.id, error)).await;
    }
  }
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

async fn query(
  pc: &TaskContext,
  query: KernelQuery,
) -> Result<KernelQueryResponse, RpcError> {
  pc.query(query).await.map_err(RpcError::internal)
}

async fn list_tasks(
  pc: &TaskContext,
  glob: Option<String>,
) -> Result<Vec<TaskInfo>, RpcError> {
  match query(
    pc,
    KernelQuery::ListTasks(TaskSpaceId::default_space(), glob),
  )
  .await?
  {
    KernelQueryResponse::TaskList(tasks) => Ok(tasks),
    _ => Err(RpcError::internal("unexpected query response")),
  }
}

async fn execute_rpc_command(
  pc: &TaskContext,
  command: Command,
) -> Result<Value, RpcError> {
  match execute(pc, &command).await.map_err(|err| match err {
    CommandError::InvalidTarget(message) => {
      RpcError::new(codes::BAD_PATH, message)
    }
    CommandError::KernelClosed => RpcError::internal(err),
  })? {
    CommandResult::Matched(matched) => {
      serde_json::to_value(ActResult { matched }).map_err(RpcError::internal)
    }
    CommandResult::None => Ok(ok_result()),
  }
}

fn parse_path(path: &str) -> Result<TaskPath, RpcError> {
  TaskPath::new(path)
    .map_err(|err| RpcError::new(codes::BAD_PATH, err.to_string()))
}

async fn handle_rpc(
  pc: &TaskContext,
  req: RpcRequest,
) -> Result<Value, RpcError> {
  match req {
    RpcRequest::TuiAttach { .. } => Err(RpcError::internal(
      "tui_attach must be the first request on a connection",
    )),

    RpcRequest::Ls { pattern } => {
      if let Some(pattern) = &pattern {
        TaskPath::check_glob(pattern)
          .map_err(|err| RpcError::new(codes::BAD_PATH, err.to_string()))?;
      }
      let tasks = list_tasks(pc, pattern)
        .await?
        .into_iter()
        .map(|t| RpcTaskInfo {
          path: t
            .path
            .map(|p| p.to_string())
            .unwrap_or_else(|| format!("<task:{}>", t.id.0)),
          label: t.label,
          state: task_state(t.state),
        })
        .collect();
      serde_json::to_value(TaskListResult { tasks }).map_err(RpcError::internal)
    }

    RpcRequest::Up { pattern } => {
      let target = match pattern {
        Some(pattern) => Target::Selector(pattern),
        None => {
          Target::Selector(format!("+{}", crate::config::task::AUTOSTART_TAG))
        }
      };
      execute_rpc_command(pc, Command::Start { target }).await
    }

    RpcRequest::Start { pattern } => {
      execute_rpc_command(
        pc,
        Command::Start {
          target: Target::Selector(pattern),
        },
      )
      .await
    }

    RpcRequest::Stop { pattern } => {
      execute_rpc_command(
        pc,
        Command::Stop {
          target: Target::Selector(pattern),
        },
      )
      .await
    }

    RpcRequest::Veto { pattern } => {
      execute_rpc_command(
        pc,
        Command::Veto {
          target: Target::Selector(pattern),
        },
      )
      .await
    }

    RpcRequest::Down { pattern } => {
      let target = match pattern {
        Some(pattern) => Target::Selector(pattern),
        None => Target::All {
          all: TaskSpaceId::default_space(),
        },
      };
      execute_rpc_command(pc, Command::Down { target }).await
    }

    RpcRequest::Kill { pattern } => {
      execute_rpc_command(
        pc,
        Command::Kill {
          target: Target::Selector(pattern),
        },
      )
      .await
    }

    RpcRequest::Restart { pattern } => {
      execute_rpc_command(
        pc,
        Command::Restart {
          target: Target::Selector(pattern),
        },
      )
      .await
    }

    RpcRequest::Why { path } => {
      let task_path = parse_path(&path)?;
      match query(pc, KernelQuery::Explain(TaskKey::default_space(task_path)))
        .await?
      {
        KernelQueryResponse::Explain(Some(explain)) => {
          let why = RpcWhy {
            path,
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
        KernelQueryResponse::Explain(None) => Err(RpcError::new(
          codes::NO_MATCH,
          format!("no task at '{}'", path),
        )),
        _ => Err(RpcError::internal("unexpected query response")),
      }
    }

    RpcRequest::Screen { path } => {
      let task_path = parse_path(&path)?;
      match query(
        pc,
        KernelQuery::GetScreen(TaskKey::default_space(task_path)),
      )
      .await?
      {
        KernelQueryResponse::Screen(Some(content)) => {
          serde_json::to_value(crate::protocol::ScreenResult {
            screen: Some(content),
          })
          .map_err(RpcError::internal)
        }
        KernelQueryResponse::Screen(None) => Err(RpcError::new(
          codes::NO_SCREEN,
          format!("no screen content for '{}'", path),
        )),
        _ => Err(RpcError::internal("unexpected query response")),
      }
    }

    RpcRequest::Shutdown {} => execute_rpc_command(pc, Command::Quit).await,

    RpcRequest::Spawn {
      path,
      cmd,
      cwd,
      env,
      deps,
      tags,
    } => {
      let task_path = parse_path(&path)?;
      if cmd.is_empty() {
        return Err(RpcError::new(
          codes::INVALID_PARAMS,
          "cmd must not be empty",
        ));
      }
      // Resolve deps to ids upfront: the kernel only accepts edges to
      // already-registered tasks, so an unknown dep is refused here.
      let mut dep_ids = Vec::with_capacity(deps.len());
      for dep in &deps {
        let dep_path = parse_path(dep)?;
        match query(
          pc,
          KernelQuery::ResolvePath(TaskKey::default_space(dep_path)),
        )
        .await?
        {
          KernelQueryResponse::ResolvedPath(Some(id)) => dep_ids.push(id),
          KernelQueryResponse::ResolvedPath(None) => {
            return Err(RpcError::new(
              codes::BAD_PATH,
              format!("no task at dep '{}'", dep),
            ));
          }
          _ => return Err(RpcError::internal("unexpected query response")),
        }
      }
      let mut spec = crate::process::process_spec::ProcessSpec::from_argv(cmd);
      if let Some(cwd) = cwd {
        spec.cwd(cwd);
      } else if let Ok(cwd) = std::env::current_dir() {
        spec.cwd(cwd.to_string_lossy());
      }
      for (k, v) in env.into_iter().flatten() {
        match v {
          Some(v) => spec.env(k, v),
          None => spec.env_remove(k),
        }
      }
      let mut cfg = crate::task::process_task::ProcessTaskConfig::new(spec);
      cfg.deps = dep_ids;
      cfg.tags = std::iter::once(crate::config::task::USER_TAG.to_string())
        .chain(tags)
        .collect();
      cfg.pinned = true;
      let (_id, ack) =
        crate::task::process_task::spawn_process_task(pc, Some(task_path), cfg);
      match ack.await {
        Ok(true) => {
          serde_json::to_value(SpawnResult { path }).map_err(RpcError::internal)
        }
        Ok(false) => Err(RpcError::new(
          codes::PATH_TAKEN,
          format!("a task already exists at '{}'", path),
        )),
        Err(_) => Err(RpcError::internal("kernel dropped the registration")),
      }
    }
  }
}
