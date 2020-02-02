#[derive(Deserialize, Debug)]
pub struct Config {
    pub working_directory: String,
    pub api_token: String,
    pub api_url: Option<String>,
    pub num_workers: u8,
    pub concurrency_per_worker: u8,
    pub public_url: String,
    pub port: u16,
    pub vm_max_inactive_secs: u64,
}

impl Config {
    pub fn total_concurrency(&self) -> u16 {
        (self.num_workers as u16) * (self.concurrency_per_worker as u16)
    }
}

#[derive(Clone, Eq, PartialEq, Deserialize, Debug)]
pub struct RootProfile {
    pub profiles: Vec<Profile>,
}

#[derive(Clone, Eq, PartialEq, Deserialize, Debug)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub image: Image,
}

#[derive(Clone, Eq, PartialEq, Deserialize, Debug)]
/// Type of a bootable image.
pub enum Image {
    /// An ISO image.
    Iso(Resource),

    /// A multiboot kernel/ramdisk/cmdline tuple.
    Multiboot {
        kernel: Resource,
        ramdisk: Option<Resource>,
        cmdline: Option<String>,
    },
}

/// Resource with a URL and a SHA-256 checksum.
#[derive(Clone, Eq, PartialEq, Deserialize, Debug)]
pub struct Resource {
    pub url: String,
    pub sha256: String,
}
