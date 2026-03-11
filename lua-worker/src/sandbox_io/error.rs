#[derive(Debug)]
pub struct SandboxIoError {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for SandboxIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "io error on '{}': {}", self.path, self.message)
    }
}

impl std::error::Error for SandboxIoError {}
