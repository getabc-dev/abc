//! [`ApiError`]: every failure as an HTTP status plus a stable `ApiCode` (D11).
//!
//! The mapping lives here, as exhaustive matches from each typed error. A new `DenialReason`,
//! `AuthError`, rejection or store error fails the build until it is given a code. The code list
//! itself is `abc_protocol::api::ApiCode`, shared with the worker and the web app.

use std::convert::Infallible;
use std::error::Error as StdError;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use abc_core::command::{RevokeTokenRejection, RevokeWorkerRejection};
use abc_core::policy::{DenialReason, Denied};
use abc_protocol::api::{ApiCode, ApiErrorBody, ApiErrorDetail};
use abc_store::{AuthError, ExecuteError, QueryError, StoreError};

/// An error response: `{"error":{"code":…,"message":…}}`, with the request id in `x-request-id`.
#[derive(Debug)]
pub(crate) struct ApiError {
    code: ApiCode,
    message: String,
    /// The cause, logged for a server error and never sent to the client.
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl ApiError {
    pub(crate) fn new(code: ApiCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            source: None,
        }
    }

    fn caused_by(mut self, source: impl StdError + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    pub(crate) fn malformed(message: impl Into<String>) -> Self {
        Self::new(ApiCode::MalformedRequest, message)
    }

    pub(crate) fn cross_site(message: impl Into<String>) -> Self {
        Self::new(ApiCode::CrossSiteRequest, message)
    }

    /// The code, for tests.
    #[cfg(test)]
    pub(crate) const fn code(&self) -> ApiCode {
        self.code
    }
}

/// The source chain of `error`, joined with `: `.
fn chain(error: &(dyn StdError + 'static)) -> String {
    let mut text = error.to_string();
    let mut next = error.source();
    while let Some(cause) = next {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        next = cause.source();
    }
    text
}

impl IntoResponse for ApiError {
    /// Logs a server error with its cause and a refusal with its code only, then renders
    /// `ApiErrorBody`.
    fn into_response(self) -> Response {
        let status = status_of(self.code);
        if status.is_server_error() {
            let cause = self.source.as_deref().map(|source| chain(source));
            tracing::error!(code = ?self.code, cause, "request failed");
        } else {
            tracing::info!(code = ?self.code, "request refused");
        }
        let body = ApiErrorBody {
            error: ApiErrorDetail {
                code: self.code,
                message: self.message,
            },
        };
        (status, Json(body)).into_response()
    }
}

/// The one status for each code.
pub(crate) const fn status_of(code: ApiCode) -> StatusCode {
    match code {
        ApiCode::MalformedRequest => StatusCode::BAD_REQUEST,
        ApiCode::CrossSiteRequest
        | ApiCode::NotAMember
        | ApiCode::WorkerOnly
        | ApiCode::SetupCodeMismatch
        | ApiCode::SetupCodeExpired
        | ApiCode::EnrolmentTokenRevoked
        | ApiCode::EnrolmentTokenUsed
        | ApiCode::EnrolmentTokenExpired => StatusCode::FORBIDDEN,
        ApiCode::NotFound | ApiCode::EnrolmentTokenUnknown | ApiCode::UnknownWorker => {
            StatusCode::NOT_FOUND
        }
        ApiCode::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
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

/// A command rejection's code. Implemented for every `Command::Rejection` type: a new command's
/// rejection type has no impl until mapped, and the `From<ExecuteError<R>>` bound below fails.
pub(crate) trait RejectionCode: StdError {
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
        Self::new(denial_code(denied.reason), denied.reason.to_string())
    }
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        Self::new(auth_code(error), error.to_string())
    }
}

impl From<StoreError> for ApiError {
    /// A failing database is `unavailable` (retryable); corrupt state or a full log is
    /// `internal`. The cause is logged, never sent.
    fn from(error: StoreError) -> Self {
        let failure = match &error {
            StoreError::Database(_) => Self::new(
                ApiCode::Unavailable,
                "the database is unavailable; retry shortly",
            ),
            StoreError::CorruptEvent { .. }
            | StoreError::Corrupt(_)
            | StoreError::Encode(_)
            | StoreError::LogFull => Self::new(
                ApiCode::Internal,
                "internal error; quote the x-request-id header when reporting it",
            ),
        };
        failure.caused_by(error)
    }
}

impl<R: RejectionCode + Send + Sync + 'static> From<ExecuteError<R>> for ApiError {
    fn from(error: ExecuteError<R>) -> Self {
        match error {
            ExecuteError::Auth(error) => error.into(),
            ExecuteError::Denied(denied) => denied.into(),
            ExecuteError::Rejected(rejection) => Self::new(rejection.code(), rejection.to_string()),
            ExecuteError::Store(error) => error.into(),
        }
    }
}

impl From<QueryError> for ApiError {
    fn from(error: QueryError) -> Self {
        match error {
            QueryError::Auth(error) => error.into(),
            QueryError::Denied(denied) => denied.into(),
            QueryError::Store(error) => error.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use abc_core::policy::{ActionName, WriteAction};

    use super::*;

    #[test]
    fn every_code_has_one_status_of_the_right_class() {
        for code in ApiCode::ALL {
            let status = status_of(code);
            let server_side = matches!(
                code,
                ApiCode::Unavailable | ApiCode::Internal | ApiCode::ServerOnly
            );
            assert_eq!(
                status.is_server_error(),
                server_side,
                "{code:?} -> {status}"
            );
            assert!(
                status.is_client_error() || status.is_server_error(),
                "{code:?} is an error"
            );
        }
    }

    #[test]
    fn denials_map_to_their_codes() {
        let denied = Denied {
            action: ActionName::Write(WriteAction::EnrolWorker),
            reason: DenialReason::EnrolmentTokenUsed,
        };
        let error = ApiError::from(denied);
        assert_eq!(error.code(), ApiCode::EnrolmentTokenUsed, "code");
        assert_eq!(
            error.message, "the enrolment token was already used",
            "message"
        );
        assert_eq!(
            ApiError::from(AuthError::WorkerRevoked).code(),
            ApiCode::WorkerRevoked,
            "auth"
        );
        assert_eq!(
            ApiError::from(ExecuteError::<RevokeTokenRejection>::Rejected(
                RevokeTokenRejection::AlreadyRedeemed
            ))
            .code(),
            ApiCode::EnrolmentTokenAlreadyRedeemed,
            "rejection"
        );
        assert_eq!(
            ApiError::from(ExecuteError::<RevokeWorkerRejection>::Rejected(
                RevokeWorkerRejection::NotFound
            ))
            .code(),
            ApiCode::NotFound,
            "rejection"
        );
    }

    #[test]
    fn store_failures_are_retryable_or_internal_and_keep_their_cause() {
        let unavailable = ApiError::from(StoreError::Database(sqlx::Error::PoolTimedOut));
        assert_eq!(
            unavailable.code(),
            ApiCode::Unavailable,
            "the database is down"
        );
        assert!(
            unavailable.source.is_some(),
            "the cause is kept for the log"
        );
        for corrupt in [StoreError::Corrupt("t.c"), StoreError::LogFull] {
            assert_eq!(ApiError::from(corrupt).code(), ApiCode::Internal, "a bug");
        }
        let error = ApiError::from(QueryError::Store(StoreError::LogFull));
        assert_eq!(error.code(), ApiCode::Internal, "through a query");
        assert_eq!(
            ApiError::from(QueryError::Auth(AuthError::UnknownWorkerKey)).code(),
            ApiCode::InvalidWorkerKey,
            "query auth"
        );
        assert_eq!(
            ApiError::from(ExecuteError::<Infallible>::Store(StoreError::LogFull)).code(),
            ApiCode::Internal,
            "through a command"
        );
        assert_eq!(
            ApiError::from(ExecuteError::<Infallible>::Auth(AuthError::WorkerRevoked)).code(),
            ApiCode::WorkerRevoked,
            "command auth"
        );
    }

    #[test]
    fn the_cause_chain_is_joined() {
        #[derive(Debug, thiserror::Error)]
        #[error("outer")]
        struct Outer(#[source] Inner);
        #[derive(Debug, thiserror::Error)]
        #[error("inner")]
        struct Inner;
        assert_eq!(
            chain(&Outer(Inner)),
            "outer: inner",
            "every cause, in order"
        );
    }
}
