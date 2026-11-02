//! Structured JSON logs (D12). One subscriber builder, used by the binary and by the tests, so the
//! test that proves no secret is logged exercises the real configuration.
//!
//! What is recorded: level, target, message, span fields (`request_id`, method, route, status,
//! latency, login, worker, session). What is never recorded: headers, raw URIs, query strings,
//! bodies, `Secret` values (their `Debug` is redacted anyway), or sqlx bind parameters (sqlx logs
//! statement text only, and its level is capped at `warn` by default).

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// The default filter when `ABC_LOG` is unset.
pub const DEFAULT_FILTER: &str = "info,sqlx=warn";

/// Builds the JSON subscriber that writes to `writer` with `filter`. The binary passes stdout; the
/// secret-log test passes an in-memory buffer at `trace`.
pub fn subscriber<W>(filter: EnvFilter, writer: W) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(false)
        .with_env_filter(filter)
        .with_writer(writer)
        .finish()
}

/// Installs the subscriber globally, writing to stdout, with `filter` (from `ABC_LOG`) or
/// [`DEFAULT_FILTER`].
///
/// # Errors
///
/// Returns [`TelemetryError`] for an invalid filter or a second installation.
pub fn init(filter: Option<&str>) -> Result<(), TelemetryError> {
    let filter =
        EnvFilter::try_new(filter.unwrap_or(DEFAULT_FILTER)).map_err(TelemetryError::Filter)?;
    tracing::subscriber::set_global_default(subscriber(filter, std::io::stdout))
        .map_err(TelemetryError::AlreadySet)
}

/// `error` and every cause under it, joined with `: `: how a failure is logged and reported.
#[must_use]
pub fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut next = error.source();
    while let Some(cause) = next {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        next = cause.source();
    }
    text
}

/// Logging could not be set up.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// `ABC_LOG` is not a valid filter.
    #[error("ABC_LOG is not a valid log filter")]
    Filter(#[source] tracing_subscriber::filter::ParseError),
    /// A global subscriber was already installed.
    #[error("logging was already initialised")]
    AlreadySet(#[source] tracing::subscriber::SetGlobalDefaultError),
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_cause_chain_is_joined() {
        #[derive(Debug, thiserror::Error)]
        #[error("outer")]
        struct Outer(#[source] Inner);
        #[derive(Debug, thiserror::Error)]
        #[error("inner")]
        struct Inner;
        assert_eq!(
            super::chain(&Outer(Inner)),
            "outer: inner",
            "every cause, in order"
        );
    }
}
