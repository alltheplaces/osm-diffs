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
//! Call sites that have actual structured data to log (numbers, an id,
//! ...) rather than just a message should attach it via the `log`
//! crate's key-value API instead of interpolating it into the message
//! text, e.g.:
//!
//! ```ignore
//! log::info!(step = "import_atp", elapsed_seconds = 12.3; "import_atp: end");
//! ```
//!
//! [`format_json`] renders those as a `"fields"` object alongside the
//! usual `timestamp`/`level`/`target`/`message`, omitted entirely for
//! records that don't attach any -- so plain `log::info!("some
//! message")` call sites (most of them, as of this writing; see e.g.
//! `pipeline::osm::assemble`, which logs failures as e.g.
//! "relation/12345" in the message text) keep working unchanged.
//! RUST_LOG still controls the level as usual, default `info`.

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
    let mut entry = serde_json::json!({
        "timestamp": timestamp,
        "level": record.level().as_str(),
        "target": record.target(),
        "message": record.args().to_string(),
    });
    let fields = key_values_to_json(record.key_values());
    if !fields.is_empty() {
        entry["fields"] = fields.into();
    }
    writeln!(buf, "{}", entry)
}

/// Converts a `log` record's key-value pairs (attached via the `log`
/// crate's key-value API, e.g. `log::info!(step = "import_atp"; "...")`)
/// into a JSON object. Uses [`log::kv::Value::visit`] rather than the
/// `kv_serde` feature: the handful of value types actually passed at
/// call sites in this crate (`u64`, `f64`, `&str`, `bool`, and `Option`
/// of those) are covered by [`log::kv::VisitValue`]'s primitive methods
/// directly, without pulling in `serde` as a `log` feature just for
/// this.
fn key_values_to_json(source: &dyn log::kv::Source) -> serde_json::Map<String, serde_json::Value> {
    struct Visitor(serde_json::Map<String, serde_json::Value>);

    impl<'kvs> log::kv::VisitSource<'kvs> for Visitor {
        fn visit_pair(
            &mut self,
            key: log::kv::Key<'kvs>,
            value: log::kv::Value<'kvs>,
        ) -> Result<(), log::kv::Error> {
            self.0.insert(key.to_string(), value_to_json(&value));
            Ok(())
        }
    }

    let mut visitor = Visitor(serde_json::Map::new());
    // A `Source`/`Value` only fails to visit if the visitor itself
    // returns an error, which ours never does.
    let _ = source.visit(&mut visitor);
    visitor.0
}

/// Converts a single `log::kv::Value` into JSON. A value this crate
/// never attaches (a captured error, a `Debug`-only type, ...) falls
/// back to its text representation via `visit_any`, rather than being
/// dropped.
fn value_to_json(value: &log::kv::Value) -> serde_json::Value {
    struct Visitor(serde_json::Value);

    impl<'v> log::kv::VisitValue<'v> for Visitor {
        fn visit_any(&mut self, value: log::kv::Value) -> Result<(), log::kv::Error> {
            self.0 = value.to_string().into();
            Ok(())
        }
        fn visit_null(&mut self) -> Result<(), log::kv::Error> {
            self.0 = serde_json::Value::Null;
            Ok(())
        }
        fn visit_u64(&mut self, value: u64) -> Result<(), log::kv::Error> {
            self.0 = value.into();
            Ok(())
        }
        fn visit_i64(&mut self, value: i64) -> Result<(), log::kv::Error> {
            self.0 = value.into();
            Ok(())
        }
        fn visit_f64(&mut self, value: f64) -> Result<(), log::kv::Error> {
            // A non-finite float (NaN, +-inf) has no JSON representation;
            // fall back to null rather than silently dropping the field
            // or failing the whole log record.
            self.0 = serde_json::Number::from_f64(value)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null);
            Ok(())
        }
        fn visit_bool(&mut self, value: bool) -> Result<(), log::kv::Error> {
            self.0 = value.into();
            Ok(())
        }
        fn visit_borrowed_str(&mut self, value: &'v str) -> Result<(), log::kv::Error> {
            self.0 = value.into();
            Ok(())
        }
    }

    let mut visitor = Visitor(serde_json::Value::Null);
    let _ = value.visit(&mut visitor);
    visitor.0
}

#[cfg(test)]
mod tests {
    use super::*;

    // `init()` sets the process-wide logger, which -- like `log::set_logger`
    // in general -- can only happen once per process; keep this the only
    // test in the crate that calls it.
    //
    // That logger then stays live for the rest of the test binary's
    // process. `cargo test` runs tests in parallel within one process, so
    // any other test that logs anything afterwards -- directly, or via a
    // dependency such as ext_sort -- gets appended to this very same file.
    // Search for our own two records by content instead of assuming we're
    // the only lines in the file, or that they're at fixed positions.
    #[test]
    fn test_init_writes_structured_startup_record() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init(dir.path())?;
        log::warn!("something worth noting: {}", "relation/12345");
        log::info!(
            step = "import_atp",
            elapsed_seconds = 12.5,
            attempt = 3u64,
            ok = true;
            "structured fields test"
        );
        let missing_rss_bytes: Option<u64> = None;
        log::info!(rss_bytes = missing_rss_bytes; "optional field test");

        let contents = std::fs::read_to_string(dir.path().join(LOG_FILE_NAME))?;
        let records: Vec<serde_json::Value> = contents
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;

        let init_module = module_path!().rsplit_once("::").unwrap().0;
        let startup = records
            .iter()
            .find(|r| r["target"] == init_module && r["level"] == "INFO")
            .with_context(|| format!("no startup record found among: {records:?}"))?;
        assert!(
            startup["message"]
                .as_str()
                .unwrap()
                .contains(env!("CARGO_PKG_VERSION")),
            "startup record should mention the binary's version: {startup}"
        );
        assert!(startup["timestamp"].as_str().is_some());

        let warn = records
            .iter()
            .find(|r| r["target"] == module_path!() && r["level"] == "WARN")
            .with_context(|| format!("no warn record found among: {records:?}"))?;
        assert!(warn["message"].as_str().unwrap().contains("relation/12345"));
        assert!(
            warn.get("fields").is_none(),
            "a record logged without any key-value pairs shouldn't get a \"fields\" key: {warn}"
        );

        let structured = records
            .iter()
            .find(|r| r["message"] == "structured fields test")
            .with_context(|| format!("no structured-fields record found among: {records:?}"))?;
        assert_eq!(structured["fields"]["step"], "import_atp");
        assert_eq!(structured["fields"]["elapsed_seconds"], 12.5);
        assert_eq!(structured["fields"]["attempt"], 3);
        assert_eq!(structured["fields"]["ok"], true);

        let optional = records
            .iter()
            .find(|r| r["message"] == "optional field test")
            .with_context(|| format!("no optional-field record found among: {records:?}"))?;
        assert_eq!(
            optional["fields"]["rss_bytes"],
            serde_json::Value::Null,
            "an absent Option field should serialize as an explicit null, \
             not be silently dropped: {optional}"
        );

        Ok(())
    }
}
