#[macro_use]
extern crate serde_derive;

mod config;
mod profile;
mod worker;

use config::Config;
use profile::ProfileManager;
use std::sync::{Arc, Mutex};
use std::{fs::File, io::Read};
use structopt::StructOpt;
use worker::Worker;

#[derive(Debug, StructOpt)]
#[structopt(name = "flat-playground-runner", about = "Flat Playground Runner")]
struct Args {
    /// Path to config file
    #[structopt(short = "c")]
    config: String,
}

#[tokio::main]
async fn main() {
    let args = Args::from_args();

    let mut config_str = String::new();
    {
        let mut config_file = File::open(&args.config).expect("cannot open config file");
        config_file
            .read_to_string(&mut config_str)
            .expect("cannot read config file");
    }
    let config: Arc<Config> =
        Arc::new(toml::from_str(&config_str).expect("cannot parse config file"));
    let pm = Arc::new(ProfileManager::new(config.clone()));
    pm.must_prepare();

    {
        let pm = pm.clone();
        tokio::spawn(async move {
            pm.run().await;
        });
    }
    pm.wait_for_root_profile().await;
    println!("Fetched root profile. Starting workers.");
    if config.concurrency == 0 {
        panic!("Concurrency must be greater than zero");
    }
    for _ in 0..config.concurrency {
        let config = config.clone();
        let pm = pm.clone();
        tokio::spawn(async move {
            let mut worker = Worker::new(config, pm);
            worker.run().await;
        });
    }
    {
        // deadlock
        let m = Mutex::new(());
        let _ = (m.lock().unwrap(), m.lock().unwrap());
    }
}
