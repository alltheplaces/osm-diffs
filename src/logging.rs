//! Structured logging to a file in the working directory.
//!
//! [`init`] wires up the `log` crate's global logger (the same facade
//! already used via `log::info!` etc. elsewhere in this crate) so that
//! every log record is appended as one JSON object per line to
//! `<workdir>/pipeline.log`, rather than printed as free-form text. That
//! makes the log machine-readable: a record's `target` says which module
//! logged it, and its `message` can be grepped for or parsed without
//! worrying about how a human-oriented formatter might wrap or color it.
//!
//! This is a first skeleton. It doesn't yet thread per-call structured
//! fields (e.g. a relation id) through the `log` crate's key-value API --
//! for now, such details go into the message text (see
//! `pipeline::osm::assemble`, which logs failures as e.g.
//! "relation/12345"). RUST_LOG still controls the level as usual, default
//! `info`.

use anyhow::{Context, Result};
use std::{fs::OpenOptions, io::Write, path::Path};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const LOG_FILE_NAME: &str = "pipeline.log";

/// Initializes the process-wide logger to append structured (JSON-lines)
/// records to `<workdir>/pipeline.log`. Must be called once, as early as
/// possible during pipeline startup, before any `log::*!` call.
pub fn init(workdir: &Path) -> Result<()> {
    let log_path = workdir.join(LOG_FILE_NAME);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open log file {}", log_path.display()))?;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(format_json)
        .target(env_logger::Target::Pipe(Box::new(file)))
        .init();

    log::info!(
        "pipeline starting up, version=v{}",
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}

/// An [`env_logger::Builder::format`] callback that renders a [`log::Record`]
/// as one JSON object, followed by a newline (the "JSON lines" convention).
fn format_json(buf: &mut env_logger::fmt::Formatter, record: &log::Record) -> std::io::Result<()> {
    // now_utc()/Rfc3339 rather than env_logger's own timestamp support,
    // to avoid depending on env_logger's "humantime" feature, which this
    // crate doesn't otherwise need.
    let timestamp = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    let entry = serde_json::json!({
        "timestamp": timestamp,
        "level": record.level().as_str(),
        "target": record.target(),
        "message": record.args().to_string(),
    });
    writeln!(buf, "{}", entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `init()` sets the process-wide logger, which -- like `log::set_logger`
    // in general -- can only happen once per process; keep this the only
    // test in the crate that calls it.
    #[test]
    fn test_init_writes_structured_startup_record() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init(dir.path())?;
        log::warn!("something worth noting: {}", "relation/12345");

        let contents = std::fs::read_to_string(dir.path().join(LOG_FILE_NAME))?;
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "expected startup + warn record: {lines:?}");

        let startup: serde_json::Value = serde_json::from_str(lines[0])?;
        assert_eq!(startup["level"], "INFO");
        assert!(
            startup["message"]
                .as_str()
                .unwrap()
                .contains(env!("CARGO_PKG_VERSION")),
            "startup record should mention the binary's version: {startup}"
        );
        assert!(startup["timestamp"].as_str().is_some());

        let warn: serde_json::Value = serde_json::from_str(lines[1])?;
        assert_eq!(warn["level"], "WARN");
        assert_eq!(warn["target"], module_path!());
        assert!(warn["message"].as_str().unwrap().contains("relation/12345"));

        Ok(())
    }
}
