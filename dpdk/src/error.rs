use thiserror::Error;

#[derive(Error, Debug)]
pub enum DpdkError {
    #[error("EAL initialization failed: {0}")]
    EalInitFailed(i32),
    #[error("Port configuration failed: {0}")]
    PortConfigFailed(i32),
    #[error("Invalid port ID: {0}")]
    InvalidPortId(u16),
    #[error("Memory allocation failed")]
    MemoryAllocationFailed,
}

pub type DpdkResult<T> = Result<T, DpdkError>;
