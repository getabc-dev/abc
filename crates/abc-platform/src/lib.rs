//! What every abc process needs from its host, shared by `abc server` and `abc worker`: the
//! operating system's entropy ([`entropy`]) and structured logs ([`telemetry`]).
//!
//! abc-core does no IO, so neither can live there; abc-store is the sqlx crate and abc-server the
//! axum crate, and the worker depends on neither.

pub mod entropy;
pub mod telemetry;
