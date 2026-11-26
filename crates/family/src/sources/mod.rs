//! Data sources feeding the dashboards.

pub mod calendar;
pub mod librus;
pub mod weather;

#[cfg(test)]
pub(crate) mod test_logs {
    /// In-memory log sink: DEBUG and up, for the life of the returned guard.
    /// Records from the `log` crate (which `rrule` uses) are captured too, as the
    /// binary's subscriber forwards them.
    #[derive(Clone, Default)]
    pub(crate) struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Logs {
        pub(crate) fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
            static BRIDGE: std::sync::Once = std::sync::Once::new();
            BRIDGE.call_once(|| {
                tracing_log::LogTracer::init().expect("no other log bridge in tests");
            });
            let logs = Self::default();
            let sink = logs.clone();
            let sub = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .with_writer(move || sink.clone())
                .finish();
            (logs, tracing::subscriber::set_default(sub))
        }

        pub(crate) fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}

/// Failure of a single source fetch.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected http status {0}")]
    Status(u16),
    /// A fixed message: parse errors never carry text from the response.
    #[error("cannot parse response: {0}")]
    Parse(&'static str),
    #[error("authentication failed: {0}")]
    Auth(String),
    /// The login POST was refused; retrying would only risk locking the account.
    #[error("credentials rejected: {0}")]
    CredentialsRejected(String),
    #[error("page is not a timetable")]
    NotTimetable,
}
