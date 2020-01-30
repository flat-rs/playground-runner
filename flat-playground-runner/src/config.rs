#[derive(Deserialize, Debug)]
pub struct Config {
    pub working_directory: String,
    pub api_token: String,
    pub concurrency: u8,
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
