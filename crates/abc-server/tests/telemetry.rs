//! Installing the global JSON logger, in a test binary of its own because it is process-wide.

#[cfg(test)]
mod tests {
    use abc_server::telemetry::{TelemetryError, init};

    #[test]
    fn init_installs_the_logger_once_and_refuses_a_bad_filter() {
        assert!(
            matches!(init(Some("[[[")), Err(TelemetryError::Filter(_))),
            "an invalid ABC_LOG is refused"
        );
        init(None).unwrap();
        tracing::info!("logged through the installed subscriber");
        assert!(
            matches!(init(Some("debug")), Err(TelemetryError::AlreadySet(_))),
            "a second installation is refused"
        );
    }
}
