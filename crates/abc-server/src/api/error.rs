//! [`ApiError`]: every failure as an HTTP status plus a stable `ApiCode` (D11).
//!
//! The mapping lives here, as exhaustive matches from each typed error. A new `DenialReason`,
//! `AuthError`, rejection or store error fails the build until it is given a code. The code list
//! itself is `abc_protocol::api::ApiCode`, shared with the worker and the web app.

use std::convert::Infallible;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use abc_core::command::{RevokeTokenRejection, RevokeWorkerRejection};
use abc_core::policy::{DenialReason, Denied};
use abc_protocol::api::ApiCode;
use abc_store::{AuthError, ExecuteError, QueryError, StoreError};

/// An error response: `{"error":{"code":…,"message":…}}` with `x-request-id` set by the layer.
#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: ApiCode,
    message: String,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: ApiCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub(crate) fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, ApiCode::NotFound, "no such route")
    }
}

impl IntoResponse for ApiError {
    /// Logs 5xx at ERROR (with the source chain) and 4xx at INFO (code only), then renders
    /// `ApiErrorBody`.
    fn into_response(self) -> Response {
        let _ = (self.status, self.code, self.message);
        unimplemented!()
    }
}

/// The one status for each code.
const fn status_of(code: ApiCode) -> StatusCode {
    match code {
        ApiCode::MalformedRequest => StatusCode::BAD_REQUEST,
        ApiCode::CrossSiteRequest
        | ApiCode::NotAMember
        | ApiCode::OwnerOnly
        | ApiCode::WorkerOnly
        | ApiCode::SetupCodeMismatch
        | ApiCode::SetupCodeExpired
        | ApiCode::EnrolmentTokenRevoked
        | ApiCode::EnrolmentTokenUsed
        | ApiCode::EnrolmentTokenExpired => StatusCode::FORBIDDEN,
        ApiCode::NotFound | ApiCode::EnrolmentTokenUnknown | ApiCode::UnknownWorker => {
            StatusCode::NOT_FOUND
        }
        ApiCode::TailnetIdentityRequired
        | ApiCode::InvalidWorkerKey
        | ApiCode::WorkerRevoked
        | ApiCode::SetupCodeRequired
        | ApiCode::EnrolmentTokenRequired => StatusCode::UNAUTHORIZED,
        ApiCode::ServerAlreadyClaimed | ApiCode::EnrolmentTokenAlreadyRedeemed => {
            StatusCode::CONFLICT
        }
        ApiCode::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        ApiCode::Internal | ApiCode::ServerOnly => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

const fn denial_code(reason: DenialReason) -> ApiCode {
    match reason {
        DenialReason::TailnetIdentityRequired => ApiCode::TailnetIdentityRequired,
        DenialReason::NotAMember => ApiCode::NotAMember,
        DenialReason::OwnerOnly => ApiCode::OwnerOnly,
        DenialReason::WorkerOnly => ApiCode::WorkerOnly,
        DenialReason::ServerAlreadyClaimed => ApiCode::ServerAlreadyClaimed,
        DenialReason::SetupCodeRequired => ApiCode::SetupCodeRequired,
        DenialReason::SetupCodeMismatch => ApiCode::SetupCodeMismatch,
        DenialReason::SetupCodeExpired => ApiCode::SetupCodeExpired,
        DenialReason::ServerOnly => ApiCode::ServerOnly,
        DenialReason::UnknownWorker => ApiCode::UnknownWorker,
        DenialReason::EnrolmentTokenRequired => ApiCode::EnrolmentTokenRequired,
        DenialReason::EnrolmentTokenUnknown => ApiCode::EnrolmentTokenUnknown,
        DenialReason::EnrolmentTokenExpired => ApiCode::EnrolmentTokenExpired,
        DenialReason::EnrolmentTokenRevoked => ApiCode::EnrolmentTokenRevoked,
        DenialReason::EnrolmentTokenUsed => ApiCode::EnrolmentTokenUsed,
    }
}

const fn auth_code(error: AuthError) -> ApiCode {
    match error {
        AuthError::UnknownWorkerKey => ApiCode::InvalidWorkerKey,
        AuthError::WorkerRevoked => ApiCode::WorkerRevoked,
    }
}

/// A command rejection's code. Implemented for every `Command::Rejection` type. A new command's
/// rejection type has no impl until mapped, and the `From<ExecuteError<R>>` bound below fails.
pub(crate) trait RejectionCode: std::error::Error {
    fn code(&self) -> ApiCode;
}

impl RejectionCode for Infallible {
    fn code(&self) -> ApiCode {
        match *self {}
    }
}

impl RejectionCode for RevokeTokenRejection {
    fn code(&self) -> ApiCode {
        match self {
            Self::NotFound => ApiCode::NotFound,
            Self::AlreadyRedeemed => ApiCode::EnrolmentTokenAlreadyRedeemed,
        }
    }
}

impl RejectionCode for RevokeWorkerRejection {
    fn code(&self) -> ApiCode {
        match self {
            Self::NotFound => ApiCode::NotFound,
        }
    }
}

impl From<Denied> for ApiError {
    fn from(denied: Denied) -> Self {
        let code = denial_code(denied.reason);
        Self::new(status_of(code), code, denied.reason.to_string())
    }
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        let code = auth_code(error);
        Self::new(status_of(code), code, error.to_string())
    }
}

impl From<StoreError> for ApiError {
    /// Database failures are `unavailable` (retryable). A corrupt event or a full log is
    /// `internal`. The source chain is logged, never sent to the client.
    fn from(error: StoreError) -> Self {
        let _ = error;
        unimplemented!()
    }
}

impl<R: RejectionCode + 'static> From<ExecuteError<R>> for ApiError {
    fn from(error: ExecuteError<R>) -> Self {
        match error {
            ExecuteError::Auth(e) => e.into(),
            ExecuteError::Denied(d) => d.into(),
            ExecuteError::Rejected(r) => {
                let code = r.code();
                Self::new(status_of(code), code, r.to_string())
            }
            ExecuteError::Store(e) => e.into(),
        }
    }
}

impl From<QueryError> for ApiError {
    fn from(error: QueryError) -> Self {
        match error {
            QueryError::Auth(e) => e.into(),
            QueryError::Denied(d) => d.into(),
            QueryError::Store(e) => e.into(),
        }
    }
}
