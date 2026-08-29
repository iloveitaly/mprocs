use std::fmt::Debug;

use serde::{Deserialize, Serialize};

use crate::{
  kernel::kernel_message::TaskSender,
  protocol::{
    ConnReceiver, ConnSender, CtlMsg, Msg, ctl::EVENT_INPUT, ok_result,
  },
  term::{ScreenDiffer, Size, TermEvent},
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ClientId(pub u32);

#[derive(Debug)]
pub enum ClientEvent {
  Input {
    client_id: ClientId,
    event: TermEvent,
  },
  Connected {
    handle: ClientHandle,
  },
  Disconnected {
    client_id: ClientId,
  },
}

pub async fn client_session(
  id: ClientId,
  app_sender: TaskSender,
  size: Size,
  request_id: u64,
  mut sender: ConnSender,
  mut receiver: ConnReceiver,
) {
  if let Err(err) = sender.send_ctl(CtlMsg::ok(request_id, ok_result())).await {
    log::warn!("client_session: failed to confirm tui_attach: {err}");
    return;
  }

  app_sender.send(ClientEvent::Connected {
    handle: ClientHandle {
      id,
      sender,
      size,
      differ: ScreenDiffer::new(),
    },
  });

  loop {
    let msg = match receiver.recv().await {
      Some(Ok(msg)) => msg,
      Some(Err(err)) => {
        log::warn!("client_session: closing: {err}");
        break;
      }
      None => break,
    };
    match msg {
      Msg::Ctl(CtlMsg::Event(event)) => {
        if event.name != EVENT_INPUT {
          log::debug!("client_session: ignoring event '{}'", event.name);
          continue;
        }
        match serde_json::from_value::<TermEvent>(event.params) {
          Ok(event) => {
            app_sender.send(ClientEvent::Input {
              client_id: id,
              event,
            });
          }
          Err(err) => {
            log::debug!("client_session: dropping input event: {err}");
          }
        }
      }
      Msg::Ctl(msg) => {
        log::debug!("client_session: ignoring message {msg:?}");
      }
      Msg::Out(_) => {
        log::debug!("client_session: ignoring output frame from client");
      }
    }
  }
  app_sender.send(ClientEvent::Disconnected { client_id: id });
}

pub struct ClientHandle {
  pub id: ClientId,
  pub sender: ConnSender,
  pub size: Size,
  pub differ: ScreenDiffer,
}

impl Debug for ClientHandle {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ClientHandle")
      .field("id", &self.id)
      .finish()
  }
}
