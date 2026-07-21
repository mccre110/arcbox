//! Error types for the API server.

use arcbox_error::CommonError;
use thiserror::Error;

/// Result type alias for API operations.
pub type Result<T> = std::result::Result<T, ApiError>;

/// Errors that can occur in API operations.
#[derive(Debug, Error)]
pub enum ApiError {
    /// Common errors (I/O, config, etc.).
    #[error(transparent)]
    Common(#[from] CommonError),

    /// Core error.
    #[error("core error: {0}")]
    Core(#[from] arcbox_core::CoreError),

    /// gRPC error.
    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::transport::Error),

    /// Server error.
    #[error("server error: {0}")]
    Server(String),

    /// Transport error.
    #[error("transport error: {0}")]
    Transport(String),
}

// Allow automatic conversion from std::io::Error to ApiError via CommonError.
impl From<std::io::Error> for ApiError {
    fn from(err: std::io::Error) -> Self {
        Self::Common(CommonError::from(err))
    }
}

/// Maps a `CommonError` (wherever it sits in the error chain) to the
/// matching gRPC status.
fn common_to_status(common: &CommonError, message: String) -> tonic::Status {
    match common {
        CommonError::Config(_) => tonic::Status::invalid_argument(message),
        CommonError::NotFound(_) => tonic::Status::not_found(message),
        CommonError::AlreadyExists(_) => tonic::Status::already_exists(message),
        CommonError::InvalidState(_) => tonic::Status::failed_precondition(message),
        CommonError::Timeout(_) => tonic::Status::deadline_exceeded(message),
        CommonError::PermissionDenied(_) => tonic::Status::permission_denied(message),
        _ => tonic::Status::internal(message),
    }
}

impl From<ApiError> for tonic::Status {
    fn from(err: ApiError) -> Self {
        let message = err.to_string();
        match &err {
            // Typed CommonErrors keep their status through the wrapping
            // layers (core, VMM) instead of collapsing to INTERNAL.
            ApiError::Common(common)
            | ApiError::Core(arcbox_core::CoreError::Common(common))
            | ApiError::Core(arcbox_core::CoreError::Vmm(arcbox_core::VmmError::Common(common))) => {
                common_to_status(common, message)
            }
            // Agent-reported errors carry an HTTP-style code over the wire.
            ApiError::Core(arcbox_core::CoreError::Agent { code, .. }) => match code {
                400 => Self::invalid_argument(message),
                404 => Self::not_found(message),
                409 => Self::already_exists(message),
                412 => Self::failed_precondition(message),
                503 => Self::unavailable(message),
                _ => Self::internal(message),
            },
            ApiError::Grpc(_) => Self::unavailable(message),
            _ => Self::internal(message),
        }
    }
}

impl ApiError {
    /// Creates a new configuration error.
    #[must_use]
    pub fn config(msg: impl Into<String>) -> Self {
        Self::Common(CommonError::config(msg))
    }
}
