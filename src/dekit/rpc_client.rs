use anyhow::bail;
use serde_json::Value;

use crate::protocol::{CtlMsg, Request, RpcRequest, client_handshake};
use crate::runner::{RunnerSpec, socket::connect_client_socket};

pub async fn rpc_request(
  runner: &RunnerSpec,
  req: RpcRequest,
  start_runner: bool,
) -> anyhow::Result<Value> {
  let (mut sender, mut receiver) =
    connect_client_socket(runner, start_runner).await?;
  client_handshake(&mut sender, &mut receiver).await?;

  let (method, params) = req.to_wire();
  sender
    .send_ctl(CtlMsg::Request(Request {
      id: 1,
      method,
      params,
    }))
    .await?;

  loop {
    match receiver.recv_ctl().await? {
      CtlMsg::Response(response) => {
        if response.id != 1 {
          continue;
        }
        match response.error {
          Some(error) => bail!("{error}"),
          None => return Ok(response.result.unwrap_or(Value::Null)),
        }
      }
      CtlMsg::Bye(bye) => bail!("runner closed the connection: {}", bye.code),
      msg @ (CtlMsg::Hello(_) | CtlMsg::Request(_) | CtlMsg::Event(_)) => {
        log::debug!("ignoring runner message {msg:?}");
      }
    }
  }
}
