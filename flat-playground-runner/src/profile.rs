use crate::config::*;
use sha2::{Digest, Sha256};
use std::{
    fs::{DirBuilder, File},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::time::delay_for;

static ROOT_PROFILE_URL: &'static str = "https://profiles.playground.flat.rs/root_profile.toml";

pub struct ProfileManager {
    config: Arc<Config>,
    root_profile: RwLock<Option<RootProfileState>>,
}

#[derive(Clone)]
struct RootProfileState {
    profile: Arc<RootProfile>,
    hash: Vec<u8>,
}

impl ProfileManager {
    pub fn new(config: Arc<Config>) -> ProfileManager {
        ProfileManager {
            config,
            root_profile: RwLock::new(None),
        }
    }

    pub fn must_prepare(&self) {
        let base_path = Path::new(&self.config.working_directory);
        DirBuilder::new()
            .recursive(true)
            .create(base_path.join("res"))
            .unwrap();
        DirBuilder::new()
            .recursive(true)
            .create(base_path.join("sock"))
            .unwrap();
    }

    pub fn get_root_profile(&self) -> Option<Arc<RootProfile>> {
        match *self.root_profile.read().unwrap() {
            Some(ref x) => Some(x.profile.clone()),
            None => None,
        }
    }

    pub async fn wait_for_root_profile(&self) {
        loop {
            let got_profile = self.root_profile.read().unwrap().is_some();
            if got_profile {
                break;
            } else {
                delay_for(Duration::from_secs(1)).await;
            }
        }
    }

    pub async fn run(&self) -> ! {
        loop {
            // Load root profile
            let root_profile_path =
                Path::new(&self.config.working_directory).join("root_profile.toml");
            if let Err(e) = try_fetch(
                root_profile_path
                    .to_str()
                    .expect("invalid bytes in root profile path"),
                ROOT_PROFILE_URL,
                None,
            ) {
                println!("root profile fetch failed: {:?}", e);
                delay_for(Duration::from_secs(10)).await;
                continue;
            }
            let mut profile_str = String::new();
            {
                let mut profile_file =
                    File::open(root_profile_path.to_str().unwrap()).expect("cannot open profile");
                profile_file
                    .read_to_string(&mut profile_str)
                    .expect("cannot read profile");
            }

            // Calculate hash of the new profile.
            let mut hasher = Sha256::new();
            hasher.input(profile_str.as_bytes());
            let new_hash = hasher.result().to_vec();

            // Only update if hash is different.
            if {
                let current = self.root_profile.read().unwrap();
                if let Some(ref current) = *current {
                    if current.hash == new_hash {
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } {
                delay_for(Duration::from_secs(120)).await;
                continue;
            }

            // Parse the new profile.
            let new_profile: Arc<RootProfile> = match toml::from_str(&profile_str) {
                Ok(x) => Arc::new(x),
                Err(e) => {
                    println!("cannot parse profile as toml: {:?}", e);
                    delay_for(Duration::from_secs(10)).await;
                    continue;
                }
            };

            // Prepare resources.
            for prof in &new_profile.profiles {
                let result = match prof.image {
                    Image::Iso(ref x) => reuse_or_fetch_resource(&*self.config, x),
                    Image::Multiboot {
                        ref kernel,
                        ref ramdisk,
                        ..
                    } => reuse_or_fetch_resource(&*self.config, kernel).and_then(|_| {
                        if let Some(ramdisk) = ramdisk {
                            reuse_or_fetch_resource(&*self.config, ramdisk)
                        } else {
                            Ok(())
                        }
                    }),
                };
                match result {
                    Ok(()) => {}
                    Err(e) => {
                        println!("resource preparation failed: {:?}", e);
                        delay_for(Duration::from_secs(10)).await;
                        continue;
                    }
                }
            }

            // Update root profile only if we have successfully fetched all required resources.
            *self.root_profile.write().unwrap() = Some(RootProfileState {
                profile: new_profile,
                hash: new_hash,
            });
            delay_for(Duration::from_secs(120)).await;
        }
    }
}

fn try_fetch(path: &str, url: &str, sha256: Option<&str>) -> Result<(), String> {
    let tmp_path = format!("{}.tmp", path);
    let status = Command::new("wget")
        .args(&["-O", &tmp_path, url])
        .status()
        .expect("cannot execute wget");
    if !status.success() {
        return Err(format!("cannot fetch from {}: {:?}", url, status));
    }
    if let Some(sha256) = sha256 {
        let real_sha256 = match file_sha256(&tmp_path) {
            Ok(x) => x,
            Err(e) => {
                drop(std::fs::remove_file(&tmp_path));
                return Err(e);
            }
        };
        if real_sha256 != sha256 {
            drop(std::fs::remove_file(&tmp_path));
            return Err(format!(
                "sha256 mismatch. got: {} expected: {}",
                real_sha256, sha256
            ));
        }
    }
    std::fs::rename(&tmp_path, path).map_err(|e| {
        format!(
            "cannot rename temporary file {} to {}: {:?}",
            tmp_path, path, e
        )
    })?;
    Ok(())
}

pub fn build_resource_path(config: &Config, sha256: &str) -> PathBuf {
    let mut path = PathBuf::new();
    path.push(&config.working_directory);
    path.push("res");
    path.push(sha256);
    path
}

fn reuse_or_fetch_resource(config: &Config, res: &Resource) -> Result<(), String> {
    let path = build_resource_path(config, &res.sha256);
    if path.exists() {
        Ok(())
    } else {
        try_fetch(
            path.to_str().expect("path contains invalid bytes"),
            &res.url,
            Some(&res.sha256),
        )?;
        Ok(())
    }
}

fn file_sha256(path: &str) -> Result<String, String> {
    let output = Command::new("sha256sum")
        .args(&[path])
        .output()
        .expect("Cannot execute sha256sum");
    if !output.status.success() {
        return Err(format!("cannot compute sha256 sum at: {}", path));
    }
    Ok(String::from_utf8(output.stdout)
        .unwrap()
        .splitn(2, " ")
        .next()
        .unwrap()
        .into())
}
