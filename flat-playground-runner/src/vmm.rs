use crate::config::*;
use futures::sink::SinkExt;
use lru_time_cache::LruCache;
use std::path::PathBuf;
use std::process::{Child as StdChild, Command as StdCommand};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::stream::StreamExt;
use tokio::sync::{
    mpsc::{channel, Receiver, Sender},
    Mutex,
};
use tokio::time::delay_for;
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{ErrorResponse, Request, Response},
        http::status::StatusCode,
        Message,
    },
    WebSocketStream,
};
use uuid::Uuid;

pub struct Vmm {
    config: Arc<Config>,
    instances: Mutex<LruCache<String, Box<Instance>>>,
    signal_channel: StdMutex<Option<InstanceSignalChannel>>,
    sender_template: Sender<()>,
}

pub struct InstanceSignalChannel {
    receiver: Receiver<()>,
}

struct InstanceGuard {
    sender: Sender<()>,
}

impl InstanceSignalChannel {
    fn new(total_concurrency: usize) -> (InstanceSignalChannel, Sender<()>) {
        let (tx, rx) = channel(total_concurrency);
        (InstanceSignalChannel { receiver: rx }, tx)
    }

    pub async fn wait(&mut self) {
        self.receiver
            .recv()
            .await
            .expect("InstanceSignalChannel::wait: Dropped?");
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        self.sender
            .try_send(())
            .expect("InstanceGuard::drop(): Unable to send signal.");
    }
}

struct Instance {
    state: QemuGuard,
    _guard: InstanceGuard,
}

struct QemuGuard {
    qemu: StdChild,
    qemu_sock: PathBuf,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ConnEndAction {
    KillInstance,
    PreserveInstance,
}

impl Drop for QemuGuard {
    fn drop(&mut self) {
        drop(self.qemu.kill());
        drop(self.qemu.wait());
        drop(std::fs::remove_file(&self.qemu_sock));
    }
}

impl Vmm {
    pub fn new(config: Arc<Config>) -> Self {
        let total_concurrency = config.total_concurrency() as usize;
        let (sigch, sender_template) = InstanceSignalChannel::new(total_concurrency);
        let ttl_secs = config.vm_max_inactive_secs;

        Vmm {
            config,
            instances: Mutex::new(LruCache::with_expiry_duration(Duration::from_secs(
                ttl_secs,
            ))),
            signal_channel: StdMutex::new(Some(sigch)),
            sender_template,
        }
    }

    pub fn take_signal_channel(&self) -> InstanceSignalChannel {
        self.signal_channel
            .try_lock()
            .expect("Vmm::signal_channel: Cannot lock signal channel.")
            .take()
            .expect("Vmm::signal_channel: Attempting to take signal channel twice.")
    }

    pub async fn must_start(self: Arc<Self>) {
        self.start().await.expect("Vmm::start() error");
    }

    pub async fn start(self: Arc<Self>) -> Result<(), String> {
        let mut listener = TcpListener::bind(("0.0.0.0", self.config.port))
            .await
            .map_err(|e| format!("Vmm::start failed on bind(): {:?}", e))?;
        {
            let me = self.clone();

            // Periodically run LRU cleanup.
            tokio::spawn(async move {
                loop {
                    delay_for(Duration::from_secs(3)).await;
                    let mut instances = me.instances.lock().await;
                    instances.get("");
                }
            });
        }

        tokio::spawn(async move {
            loop {
                let (stream, _) = listener
                    .accept()
                    .await
                    .expect("Vmm::listen: accept() failed");
                let me = self.clone();
                tokio::spawn(async move {
                    match me.handle_incoming(stream).await {
                        Ok(()) => {}
                        Err(e) => {
                            println!("handle_incoming(): {:?}", e);
                        }
                    }
                });
            }
        });
        Ok(())
    }

    async fn handle_incoming(&self, stream: TcpStream) -> Result<(), String> {
        let mut instances = self.instances.lock().await;

        let mut instance_id: Option<String> = None;
        let mut instance: Option<Box<Instance>> = None;

        let socket = accept_hdr_async(stream, |req: &Request, res: Response| {
            let path = req.uri().path();
            if path.len() == 0 {
                let mut response = ErrorResponse::new(Some("bad request".into()));
                *response.status_mut() = StatusCode::BAD_REQUEST;
                return Err(response);
            }
            let path = &path[1..];
            if let Some(_instance) = instances.remove(path) {
                instance_id = Some(path.into());
                instance = Some(_instance);
                Ok(res)
            } else {
                let mut response = ErrorResponse::new(Some("session not found".into()));
                *response.status_mut() = StatusCode::NOT_FOUND;
                Err(response)
            }
        })
        .await
        .map_err(|e| format!("accept_hdr_async: {:?}", e))?;

        drop(instances);

        let instance_id = instance_id.unwrap();
        let instance = instance.unwrap();

        let action = self.handle_incoming_stage_2(socket, &instance).await;

        match action {
            ConnEndAction::KillInstance => {}
            ConnEndAction::PreserveInstance => {
                self.insert_instance(instance_id, instance).await;
            }
        }

        Ok(())
    }

    async fn handle_incoming_stage_2(
        &self,
        mut ws: WebSocketStream<TcpStream>,
        instance: &Instance,
    ) -> ConnEndAction {
        let mut qemu_sock = match UnixStream::connect(&instance.state.qemu_sock).await {
            Ok(x) => x,
            Err(e) => {
                println!("handle_incoming_stage_2: Cannot connect to qemu: {:?}", e);
                return ConnEndAction::KillInstance;
            }
        };

        let mut buf: Vec<u8> = vec![0; 16384];
        loop {
            let ws_fut = ws.next();
            let qemu_fut = qemu_sock.read(buf.as_mut_slice());
            tokio::select! {
                msg = ws_fut => {
                    match msg {
                        None => return ConnEndAction::PreserveInstance,
                        Some(Ok(Message::Binary(x))) => {
                            match qemu_sock.write_all(&x).await {
                                Ok(_) => {},
                                Err(e) => {
                                    println!("handle_incoming_stage_2: cannot write to qemu: {:?}", e);
                                    return ConnEndAction::KillInstance;
                                }
                            }
                        }
                        Some(Ok(Message::Text(_))) => {
                            println!("handle_incoming_stage_2: text message is not supported");
                            return ConnEndAction::PreserveInstance;
                        }
                        Some(Ok(_)) => {},
                        Some(Err(e)) => {
                            println!("handle_incoming_stage_2: read failed: {:?}", e);
                            return ConnEndAction::PreserveInstance;
                        }
                    }
                }
                msg = qemu_fut => {
                    let n = match msg {
                        Ok(x) => x,
                        Err(e) => {
                            println!("handle_incoming_stage_2: qemu read error: {:?}", e);
                            return ConnEndAction::KillInstance;
                        }
                    };
                    if n == 0 {
                        return ConnEndAction::KillInstance;
                    }
                    match ws.send(Message::Binary(buf[..n].to_vec())).await {
                        Ok(_) => {},
                        Err(e) => {
                            println!("handle_incoming_stage_2: ws send error: {:?}", e);
                            return ConnEndAction::PreserveInstance;
                        }
                    }
                }
            }
        }
    }

    pub async fn start_instance(&self, id: String, profile: &Profile) -> Result<(), String> {
        let qemu = self.start_qemu(&id, profile).await?;
        self.insert_instance(
            id,
            Box::new(Instance {
                state: qemu,
                _guard: InstanceGuard {
                    sender: self.sender_template.clone(),
                },
            }),
        )
        .await;
        Ok(())
    }

    async fn insert_instance(&self, id: String, instance: Box<Instance>) {
        self.instances.lock().await.insert(id, instance);
    }

    async fn start_qemu(&self, id: &str, profile: &Profile) -> Result<QemuGuard, String> {
        let id = Uuid::parse_str(id).map_err(|_| "start_qemu: Cannot parse task id".to_string())?;

        let mut args: Vec<&str> = vec![
            "--enable-kvm",
            "-m",
            "256M",
            "-smp",
            "2",
            "-device",
            "intel-iommu,intremap=on",
            "-machine",
            "q35,kernel-irqchip=split",
            "-cpu",
            "host",
        ];

        let sockpath = build_sock_path(&*self.config, &id);

        let buf_sockpath = format!("unix:{}", sockpath.to_str().unwrap());
        args.push("-vnc");
        args.push(&buf_sockpath);

        let buf_iso;
        let buf_kernel;
        let buf_ramdisk;

        match profile.image {
            Image::Iso(ref iso) => {
                args.push("-cdrom");
                buf_iso = crate::profile::build_resource_path(&*self.config, &iso.sha256)
                    .to_str()
                    .unwrap()
                    .to_string();
                args.push(&buf_iso);
            }
            Image::Multiboot {
                ref kernel,
                ref ramdisk,
                ref cmdline,
            } => {
                args.push("-kernel");
                buf_kernel = crate::profile::build_resource_path(&*self.config, &kernel.sha256)
                    .to_str()
                    .unwrap()
                    .to_string();
                args.push(&buf_kernel);

                if let Some(ref ramdisk) = *ramdisk {
                    args.push("-initrd");
                    buf_ramdisk =
                        crate::profile::build_resource_path(&*self.config, &ramdisk.sha256)
                            .to_str()
                            .unwrap()
                            .to_string();
                    args.push(&buf_ramdisk);
                }

                if let Some(ref cmdline) = *cmdline {
                    args.push("-append");
                    args.push(cmdline);
                }
            }
        }

        drop(std::fs::remove_file(&sockpath));

        let mut child = StdCommand::new("qemu-system-x86_64")
            .args(&args)
            .spawn()
            .map_err(|e| format!("cannot start qemu: {:?}", e))?;

        loop {
            delay_for(Duration::from_millis(50)).await;
            if sockpath.exists() {
                break;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    return Err(format!("qemu early exit: {:?}", status));
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(format!("error on qemu try_wait: {:?}", e));
                }
            }
        }
        Ok(QemuGuard {
            qemu: child,
            qemu_sock: sockpath,
        })
    }
}

fn build_sock_path(config: &Config, id: &Uuid) -> PathBuf {
    let mut path = PathBuf::new();
    path.push(&config.working_directory);
    path.push("sock");
    path.push(&format!("{}", id.to_hyphenated_ref()));
    path
}
