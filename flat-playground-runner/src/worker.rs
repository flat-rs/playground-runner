// kvm -vnc unix:/home/services/vms/vnc-socket

use crate::config::*;
use crate::profile::ProfileManager;
use crate::vmm::{InstanceSignalChannel, Vmm};
use futures::{
    sink::{Sink, SinkExt},
    stream::StreamExt,
};
use serde_json::json;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::marker::Unpin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::delay_for;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

static DEFAULT_API_URL: &'static str = "wss://eu-1.runners.playground.flat.rs/v1";

pub struct Worker {
    config: Arc<Config>,
    pm: Arc<ProfileManager>,
    vmm: Arc<Vmm>,
    wfr: VecDeque<WaitForResult>,
}

#[derive(Clone, Debug, Deserialize)]
struct Confirmation {
    profile: String,
}

#[derive(Clone, Debug, Deserialize)]
enum ApiMessage {
    ClientTask(ClientTask),
    Result(ResultMessage),
    Pong,
}

#[derive(Clone, Debug, Deserialize)]
struct ClientTask {
    id: String,
    profile: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ResultMessage {
    status: i32,
    message: String,
}

enum WaitForResult {
    ConfirmClient,
    DropClient,
    UpdateCapacity,
}

impl Worker {
    pub fn new(config: Arc<Config>, pm: Arc<ProfileManager>, vmm: Arc<Vmm>) -> Worker {
        Worker {
            config,
            pm,
            vmm,
            wfr: VecDeque::new(),
        }
    }

    pub async fn run(&mut self) {
        let mut vmm_signal = self.vmm.take_signal_channel();

        loop {
            match self.run_once(&mut vmm_signal).await {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("Error: {}", e);
                    delay_for(Duration::from_secs(1)).await;
                }
            }
        }
    }

    async fn update_capacity<S: Sink<Message> + Unpin>(
        &mut self,
        sink: &mut S,
        diff: i64,
    ) -> Result<(), String>
    where
        S::Error: Debug,
    {
        let req = serde_json::to_string(&json!({
            "type": "update_capacity",
            "diff": diff,
        }))
        .unwrap();
        sink.send(Message::Text(req))
            .await
            .map_err(|e| format!("update_capacity: cannot send message: {:?}", e))?;
        self.wfr.push_back(WaitForResult::UpdateCapacity);
        Ok(())
    }

    async fn drop_client<S: Sink<Message> + Unpin>(
        &mut self,
        sink: &mut S,
        id: &str,
    ) -> Result<(), String>
    where
        S::Error: Debug,
    {
        let req = serde_json::to_string(&json!({
            "type": "drop_client",
            "clientId": id,
        }))
        .unwrap();
        sink.send(Message::Text(req))
            .await
            .map_err(|e| format!("drop_client: cannot send message: {:?}", e))?;
        self.wfr.push_back(WaitForResult::DropClient);
        Ok(())
    }

    async fn confirm_client<S: Sink<Message> + Unpin>(
        &mut self,
        sink: &mut S,
        id: &str,
    ) -> Result<(), String>
    where
        S::Error: Debug,
    {
        let req = serde_json::to_string(&json!({
            "type": "confirm_client",
            "clientId": id,
            "url": format!("{}/{}", self.config.public_url, id),
        }))
        .unwrap();
        sink.send(Message::Text(req))
            .await
            .map_err(|e| format!("confirm_client: cannot send message: {:?}", e))?;
        self.wfr.push_back(WaitForResult::ConfirmClient);
        Ok(())
    }

    async fn send_ping<S: Sink<Message> + Unpin>(&self, sink: &mut S) -> Result<(), String>
    where
        S::Error: Debug,
    {
        let req = serde_json::to_string(&json!({
            "type": "ping",
        }))
        .unwrap();
        sink.send(Message::Text(req))
            .await
            .map_err(|e| format!("send_ping: cannot send message: {:?}", e))?;
        Ok(())
    }

    async fn run_once(&mut self, vmm_signal: &mut InstanceSignalChannel) -> Result<(), String> {
        let api_url = self
            .config
            .api_url
            .as_ref()
            .map(|x| x.as_str())
            .unwrap_or(DEFAULT_API_URL);
        let api_url = format!("{}?token={}", api_url, self.config.api_token);
        let parsed_url = Url::parse(&api_url).unwrap();
        let (mut socket, _) = connect_async(parsed_url)
            .await
            .map_err(|e| format!("Cannot connect to server: {:?}", e))?;

        println!("Connected.");

        self.update_capacity(&mut socket, self.config.concurrency_per_worker as _)
            .await?;

        let mut ping_sent = false;

        loop {
            let socket_next_fut = socket.next();
            let vmm_signal_fut = vmm_signal.wait();
            let delay_fut = delay_for(Duration::from_secs(60));

            tokio::select! {
                msg = socket_next_fut => {
                    let msg = match msg {
                        None => return Ok(()),
                        Some(Ok(Message::Binary(_))) => {
                            return Err(format!("unexpected binary message"));
                        }
                        Some(Ok(Message::Text(x))) => x,
                        Some(Ok(_)) => {
                            continue;
                        },
                        Some(Err(e)) => {
                            return Err(format!("cannot read from websocket: {:?}", e));
                        }
                    };
                    let msg: ApiMessage = serde_json::from_str(&msg)
                        .map_err(|e| format!("Cannot deserialize API message: {:?}", e))?;
                    match msg {
                        ApiMessage::Pong => {
                            if !ping_sent {
                                println!("Warning: Got Pong without a Ping");
                            } else {
                                ping_sent = false;
                            }
                        }
                        ApiMessage::Result(result) => {
                            let wfr = self.wfr.pop_front();
                            match wfr {
                                None => println!("Warning: Got Result without a previous request"),
                                Some(WaitForResult::ConfirmClient) => {
                                    if result.status != 0 {
                                        println!("ConfirmClient failed: {}", result.message);
                                    }
                                }
                                Some(WaitForResult::DropClient) => {
                                    if result.status != 0 {
                                        println!("DropClient failed: {}", result.message);
                                    }
                                }
                                Some(WaitForResult::UpdateCapacity) => {
                                    if result.status != 0 {
                                        println!("UpdateCapacity failed: {}", result.message);
                                    }
                                }
                            }
                        }
                        ApiMessage::ClientTask(task) => {
                            let root_profile = self
                                .pm
                                .get_root_profile()
                                .expect("expecting root profile to be ready");
                            let profile = match root_profile
                                .profiles
                                .iter()
                                .find(|x| x.id == task.profile)
                            {
                                Some(x) => x,
                                None => {
                                    println!("Profile not found: {}", task.profile);
                                    self.drop_client(&mut socket, &task.id).await?;
                                    continue;
                                }
                            };
                            if let Err(e) = self.vmm.start_instance(task.id.clone(), profile).await {
                                println!("Cannot start instance: {:?}", e);
                                self.drop_client(&mut socket, &task.id).await?;
                                continue;
                            }
                            self.confirm_client(&mut socket, &task.id).await?;
                        }
                    }
                }
                _ = vmm_signal_fut => {
                    self.update_capacity(&mut socket, 1).await?;
                }
                _ = delay_fut => {
                    if ping_sent {
                        return Err("Ping timeout".into());
                    } else {
                        ping_sent = true;
                        self.send_ping(&mut socket).await?;
                    }
                }
            }
        }
    }
}
