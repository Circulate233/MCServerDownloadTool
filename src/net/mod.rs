//! Shared blocking HTTP transport and high-performance artifact transfer engine.
//!
//! API and metadata responses use [`HttpTransport`]. Every filesystem download,
//! including a single file, uses [`ArtifactTransfer`] so connection pooling,
//! retries, concurrency limits, progress, verification, and atomic publication
//! have one implementation.

mod model;
mod pool;
mod transfer;
mod transport;

pub use model::{
    ArtifactOutcome, ArtifactRequest, ArtifactRequestBuilder, DownloadMode, HttpRequest,
    HttpRequestBuilder, HttpResponse, NetworkConfig, NetworkError, NetworkLimits, SensitiveHeaders,
    TransferError, TransferEvent, TransferObserver, TransferObserverError, TransferPhase,
};
pub use transfer::{ArtifactTransfer, NetworkEngine};
pub use transport::HttpTransport;
