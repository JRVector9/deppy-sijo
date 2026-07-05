#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProcessIdentity {
    pub pid: Option<u32>,
    pub process_group: Option<u32>,
    pub source: ProcessIdentitySource,
}

impl ProcessIdentity {
    pub fn unavailable() -> Self {
        Self {
            pid: None,
            process_group: None,
            source: ProcessIdentitySource::Unavailable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProcessIdentitySource {
    PortablePty,
    PlatformFallback,
    Unavailable,
}
