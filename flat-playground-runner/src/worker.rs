// kvm -vnc unix:/home/services/vms/vnc-socket

use crate::config::*;
use crate::profile::ProfileManager;
use futures::compat::{Future01CompatExt, Sink01CompatExt, Stream01CompatExt};
use futures::{sink::SinkExt, stream::StreamExt};
use futures_01::stream::Stream;
use std::path::PathBuf;
use std::process::{Child as StdChild, Command as StdCommand};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::delay_for;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use url::Url;
use uuid::Uuid;

static API_URL: &'static str = "wss://playground-ws.x.flat.rs/cloudvm";

pub struct Worker {
    config: Arc<Config>,
    pm: Arc<ProfileManager>,
    id: Uuid,
}

#[derive(Clone, Debug, Deserialize)]
struct Confirmation {
    profile: String,
}

struct QemuGuard {
    qemu: StdChild,
    qemu_sock: PathBuf,
}

impl Drop for QemuGuard {
    fn drop(&mut self) {
        drop(self.qemu.kill());
        drop(self.qemu.wait());
        drop(std::fs::remove_file(&self.qemu_sock));
    }
}

impl Worker {
    pub fn new(config: Arc<Config>, pm: Arc<ProfileManager>) -> Worker {
        Worker {
            config,
            pm,
            id: Uuid::new_v4(),
        }
    }

    pub async fn run(&mut self) {
        loop {
            match self.run_once().await {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("Error: {}", e);
                    delay_for(Duration::from_secs(1)).await;
                }
            }
        }
    }

    async fn run_once(&mut self) -> Result<(), String> {
        let api_url = format!("{}?type=backend&token={}", API_URL, self.config.api_token);
        let parsed_url = Url::parse(&api_url).unwrap();
        let (socket, _) = connect_async(parsed_url)
            .compat()
            .await
            .map_err(|e| format!("Cannot connect to server: {:?}", e))?;
        let mut socket = socket.compat();

        println!("Connected.");

        // Wait for request
        let msg = match tokio::time::timeout(Duration::from_secs(60), socket.next()).await {
            Ok(Some(Ok(x))) => x,
            Ok(Some(Err(e))) => {
                return Err(format!("WFR error: {:?}", e));
            }
            Ok(None) => {
                return Err(format!("WFR: Connection closed"));
            }
            Err(_) => {
                println!("WFR: Timed out. Retrying.");
                return Ok(());
            }
        };

        let confirmation: Confirmation = match msg {
            Message::Text(t) => serde_json::from_str(&t)
                .map_err(|e| format!("Cannot decode confirmation: {:?}", e))?,
            _ => return Err(format!("Unexpected confirmation message: {:?}", msg)),
        };
        println!("Profile: {}", confirmation.profile);
        let root_profile = self
            .pm
            .get_root_profile()
            .expect("expecting root profile to be ready");
        let profile = match root_profile
            .profiles
            .iter()
            .find(|x| x.id == confirmation.profile)
        {
            Some(x) => x,
            None => return Err(format!("Cannot find profile: {}", confirmation.profile)),
        };
        let qemu = self.start_qemu(profile).await?;
        let mut qemu_sock = UnixStream::connect(&qemu.qemu_sock)
            .await
            .map_err(|e| format!("Cannot connect to qemu: {:?}", e))?;

        let (ws_write, ws_read) = socket.into_inner().split();
        let (mut qemu_read, mut qemu_write) = qemu_sock.split();

        let mut ws_write = ws_write.sink_compat();
        let mut ws_read = ws_read.compat();

        let mut buf: Vec<u8> = vec![0; 16384];

        loop {
            let mut ws_read = ws_read.next();
            let mut qemu_read = qemu_read.read(buf.as_mut_slice());
            tokio::select! {
                msg = &mut ws_read => {
                    match msg {
                        None => return Ok(()),
                        Some(Ok(Message::Binary(_))) => {
                            return Err(format!("binary message is not supported"));
                        }
                        Some(Ok(Message::Text(x))) => {
                            let decoded = base64::decode(&x)
                                .map_err(|_| format!("cannot decode as base64"))?;
                            qemu_write.write(&decoded).await
                                .map_err(|e| format!("cannot send message to qemu: {:?}", e))?;
                        }
                        Some(Ok(_)) => {},
                        Some(Err(e)) => {
                            return Err(format!("cannot read from ws: {:?}", e));
                        }
                    }
                }
                msg = &mut qemu_read => {
                    match msg {
                        Ok(n) => {
                            if n == 0 {
                                println!("qemu exited");
                                return Ok(());
                            }
                            ws_write.send(Message::Text(base64::encode(&buf[..n]))).await
                                .map_err(|e| format!("cannot send message to ws: {:?}", e))?;
                        },
                        Err(e) => return Err(format!("qemu read error: {:?}", e)),
                    }
                }
            }
        }
    }
    async fn start_qemu(&mut self, profile: &Profile) -> Result<QemuGuard, String> {
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

        let sockpath = build_sock_path(&*self.config, &self.id);

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
