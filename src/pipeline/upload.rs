//! Uploads pipeline outputs to S3-compatible storage.
//!
//! Every upload goes through [upload_file], which decides between a
//! single PUT and a multi-part upload based on the file's size (see
//! [MULTIPART_THRESHOLD]) -- multi-part's own per-part minimum makes it
//! pure overhead for something as small as a log file, but it's still
//! the right choice for larger files like tiles or `conflated.parquet`.
//!
//! All uploads are skipped (not an error) if `S3_ENDPOINT` isn't set --
//! the established way to disable uploads for a local/dev run.

use crate::make_download_bar;
use anyhow::{Context, Result};
use indicatif::MultiProgress;
use std::{env, fs::File, io::Read, path::Path};
use time::UtcDateTime;

/// Below this size, [upload_file] PUTs the whole object in one request
/// instead of a multi-part upload.
const MULTIPART_THRESHOLD: u64 = 8 * 1024 * 1024;

/// Size of each part in a multi-part upload. Some S3 implementations
/// require at least 5 MiB for all parts except the last.
const PART_SIZE: usize = 8 * 1024 * 1024;

/// Helper to perform a multi-part upload to S3 storage.
///
/// If the helper gets dropped before finish(), the drop() method
/// will send a request to the S3 server to abort the pending upload.
struct Upload<'a> {
    client: &'a s3::BlockingClient,
    bucket: &'a str,
    destination: &'a str,
    upload_id: String,
    parts: Vec<s3::types::CompletedPart>,
}

impl<'a> Upload<'a> {
    fn create(
        client: &'a s3::BlockingClient,
        bucket: &'a str,
        destination: &'a str,
        content_type: &str,
    ) -> Result<Upload<'a>> {
        let upload = client
            .objects()
            .create_multipart_upload(bucket, destination)
            .content_type(content_type)?
            .send()
            .context("create_multipart_upload failed")?;

        Ok(Upload {
            client,
            bucket,
            destination,
            upload_id: upload.upload_id,
            parts: Vec::new(),
        })
    }

    fn upload_part(&mut self, buf: Vec<u8>) -> Result<()> {
        let part_num = (self.parts.len() + 1) as u32;
        let response = self
            .client
            .objects()
            .upload_part(self.bucket, self.destination, &self.upload_id, part_num)
            .body_bytes(buf)
            .send()
            .with_context(|| format!("upload_part {} failed", part_num))?;
        if let Some(etag) = response.etag {
            self.parts
                .push(s3::types::CompletedPart::new(part_num, etag)?);
        } else {
            anyhow::bail!("no etag for part {}", part_num);
        }
        Ok(())
    }

    /// Complete the multi-part upload. Returns the etag of the created file.
    fn finish(&mut self) -> Result<Option<String>> {
        let result = self
            .client
            .objects()
            .complete_multipart_upload(self.bucket, self.destination, &self.upload_id)
            .parts(self.parts.clone())?
            .send()?;
        self.parts.clear();
        Ok(result.etag)
    }
}

impl<'a> Drop for Upload<'a> {
    fn drop(&mut self) {
        if !self.parts.is_empty() {
            _ = self
                .client
                .objects()
                .abort_multipart_upload(self.bucket, self.destination, &self.upload_id)
                .send();
        }
    }
}

/// S3 connection details, read once from the environment and reused
/// for every upload in a pipeline run.
///
/// All five come from environment variables, none of them CLI flags
/// (`osm-diffs run --help` won't mention them) -- they're ambient
/// deployment config, the kind you'd set once for wherever this runs
/// on a schedule, not something to pass per invocation:
///
/// - `S3_ENDPOINT` -- the S3-compatible service's base URL (e.g.
///   `https://s3.amazonaws.com`, or a MinIO/other provider's own URL).
///   Also the on/off switch: if this is unset, every upload in this
///   module is skipped entirely (see [S3Config::from_env]) -- the
///   established way to disable uploads for a local/dev run.
/// - `S3_BUCKET` -- the bucket every upload in this module writes to
///   (`edits.pmtiles`, `conflated.parquet`, `logs/<run-id>.log` all
///   land in the same one, distinguished by key).
/// - `S3_REGION` -- passed to the S3 client as-is; some S3-compatible
///   services ignore it, but the client still requires a value.
/// - `S3_ACCESS_KEY_ID` / `S3_ACCESS_KEY_SECRET` -- static credentials
///   for that bucket.
struct S3Config {
    endpoint: String,
    bucket: String,
    region: String,
    access_key_id: String,
    access_key_secret: String,
}

impl S3Config {
    /// Reads `S3_ENDPOINT`/`S3_BUCKET`/`S3_REGION`/`S3_ACCESS_KEY_ID`/
    /// `S3_ACCESS_KEY_SECRET` from the environment. Returns `Ok(None)`
    /// (not an error) if `S3_ENDPOINT` specifically is unset -- that's
    /// how uploads are deliberately disabled. Once `S3_ENDPOINT` *is*
    /// set, every other variable missing is a real configuration error.
    fn from_env() -> Result<Option<S3Config>> {
        let Some(endpoint) = env::var("S3_ENDPOINT").ok() else {
            return Ok(None);
        };
        Ok(Some(S3Config {
            endpoint,
            bucket: env_var("S3_BUCKET")?,
            region: env_var("S3_REGION")?,
            access_key_id: env_var("S3_ACCESS_KEY_ID")?,
            access_key_secret: env_var("S3_ACCESS_KEY_SECRET")?,
        }))
    }

    fn client(&self) -> Result<s3::BlockingClient> {
        let auth = s3::Auth::Static(s3::Credentials::new(
            self.access_key_id.clone(),
            self.access_key_secret.clone(),
        )?);
        // Real AWS wants virtual-hosted-style addressing (bucket.s3.
        // amazonaws.com/key); anything else (MinIO, mockito in tests,
        // ...) gets path-style (host/bucket/key), which is what
        // non-AWS S3-compatible servers generally expect.
        let addressing = if self.endpoint.contains("amazonaws.com") {
            s3::AddressingStyle::Auto
        } else {
            s3::AddressingStyle::Path
        };
        Ok(s3::BlockingClient::builder(&self.endpoint)?
            .region(&self.region)
            .auth(auth)
            .addressing_style(addressing)
            .build()?)
    }
}

fn env_var(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("Missing environment variable: {name}"))
}

/// Uploads `path` to `destination` in the configured S3 bucket. Skips
/// entirely (logging why) if `S3_ENDPOINT` isn't set. Chooses a single
/// PUT or a multi-part upload based on `path`'s size.
fn upload_file(
    path: &Path,
    destination: &str,
    content_type: &str,
    progress_label: &str,
    progress: &MultiProgress,
) -> Result<()> {
    upload_file_with_config(
        S3Config::from_env()?.as_ref(),
        path,
        destination,
        content_type,
        progress_label,
        progress,
    )
}

/// Does the actual work for [upload_file], taking an already-resolved
/// `config` rather than reading the environment itself -- split out
/// so tests can inject a config pointing at a mock server directly,
/// instead of going through process-global environment variables
/// (which `cargo test`'s parallel execution makes an awkward, racy
/// thing to share between tests).
fn upload_file_with_config(
    config: Option<&S3Config>,
    path: &Path,
    destination: &str,
    content_type: &str,
    progress_label: &str,
    progress: &MultiProgress,
) -> Result<()> {
    let Some(config) = config else {
        log::warn!("S3_ENDPOINT not set, skipping upload of {destination}");
        return Ok(());
    };
    let client = config.client()?;

    let mut file = File::open(path).with_context(|| format!("cannot open {path:?}"))?;
    let num_bytes = file.metadata()?.len();
    let progress_bar = make_download_bar(progress, progress_label, Some(num_bytes));

    let etag = if num_bytes < MULTIPART_THRESHOLD {
        let mut body = Vec::with_capacity(num_bytes as usize);
        file.read_to_end(&mut body)
            .with_context(|| format!("cannot read {path:?}"))?;
        let result = client
            .objects()
            .put(&config.bucket, destination)
            .content_type(content_type)?
            .body_bytes(body)
            .send()
            .with_context(|| format!("put_object {destination} failed"))?;
        progress_bar.inc(num_bytes);
        result.etag
    } else {
        let mut upload = Upload::create(&client, &config.bucket, destination, content_type)?;
        let mut buf = vec![0u8; PART_SIZE];
        loop {
            let mut bytes_read = 0usize;
            loop {
                let n = file
                    .read(&mut buf[bytes_read..])
                    .context("File read error")?;
                if n == 0 {
                    break;
                } // EOF
                bytes_read += n;
                if bytes_read == PART_SIZE {
                    break;
                } // buffer full
            }
            if bytes_read == 0 {
                break;
            } // nothing left
            let chunk = buf[..bytes_read].to_vec();
            upload.upload_part(chunk)?;
            progress_bar.inc(bytes_read as u64);
        }
        upload.finish()?
    };

    progress_bar.finish();
    // `s3://bucket/key` isn't an IANA/IETF-registered URI scheme -- there
    // isn't one for S3-compatible object storage -- but it's the de
    // facto convention across the ecosystem (AWS CLI, boto3, Terraform,
    // Hadoop's S3A, ...), and endpoint-agnostic besides (unlike an
    // https:// URL, which would depend on how a given S3-compatible
    // provider maps buckets to hostnames/paths, if it exposes public
    // HTTPS access at all). One log line rather than separate
    // endpoint/bucket/destination fields, since this one value already
    // says everything a reader needs to find the object.
    log::info!(
        s3_url = format!("s3://{}/{}", config.bucket, destination),
        content_type = content_type,
        bytes = num_bytes,
        etag = etag;
        "upload_file: done"
    );
    Ok(())
}

pub fn upload_tiles(tiles: &Path, progress: &MultiProgress) -> Result<()> {
    upload_file(
        tiles,
        "edits.pmtiles",
        "application/vnd.pmtiles",
        "upload.tiles",
        progress,
    )
}

pub fn upload_conflated(conflated: &Path, progress: &MultiProgress) -> Result<()> {
    upload_file(
        conflated,
        "conflated.parquet",
        "application/vnd.apache.parquet",
        "upload.conflated",
        progress,
    )
}

pub fn upload_conflated_tiles(tiles: &Path, progress: &MultiProgress) -> Result<()> {
    upload_file(
        tiles,
        "conflated.pmtiles",
        "application/vnd.pmtiles",
        "upload.conflated-tiles",
        progress,
    )
}

/// Uploads `workdir`'s `pipeline.log` to `logs/<run-id>.log`, where
/// `<run-id>` is `pipeline_start_time` -- the same timestamp already
/// embedded into `conflated.parquet`'s provenance BOM as
/// `formulation[].workflows[].timeStart` (see `pipeline::provenance`), so
/// a given run's log and its data output can always be tied back
/// together. See [docs/LOGGING.md](../../../docs/LOGGING.md) for the
/// user-facing explanation of this layout.
pub fn upload_logs(
    workdir: &Path,
    pipeline_start_time: UtcDateTime,
    progress: &MultiProgress,
) -> Result<()> {
    let log_path = workdir.join("pipeline.log");
    let destination = format!("logs/{}.log", run_id(pipeline_start_time)?);
    // TODO: We should use an official, IANA-assigned content type for
    // JSON Lines here, but as of August 2026, no consensus has yet been
    // reached on what string to use, so the registration appears to be
    // stalled -- see https://github.com/wardi/jsonlines/issues/19.
    // Tracked in alltheplaces/osm-diffs#684 to check back in August 2027.
    upload_file(
        &log_path,
        &destination,
        "application/x-ndjson",
        "upload.logs",
        progress,
    )
}

/// Formats a timestamp as `YYYY-MM-DD-HH-MM-SS` -- filesystem/URL-safe
/// (no colons, unlike RFC 3339), for use as an S3 key component.
fn run_id(t: UtcDateTime) -> Result<String> {
    let format = time::format_description::parse_borrowed::<2>(
        "[year]-[month]-[day]-[hour]-[minute]-[second]",
    )
    .context("bad run-id format description")?;
    t.format(&format).context("could not format run id")
}

#[cfg(test)]
mod tests {
    use super::*;
    use indicatif::ProgressDrawTarget;
    use mockito::Server;
    use tempfile::TempDir;

    /// Builds a config pointing at `server`, for tests to inject
    /// directly via [upload_file_with_config] -- deliberately not going
    /// through `S3Config::from_env`/process env vars at all, since those
    /// are global, mutable state that `cargo test`'s parallel execution
    /// makes unsafe to share between tests.
    fn test_config(server: &Server) -> S3Config {
        S3Config {
            endpoint: server.url(),
            bucket: "test-bucket".to_string(),
            region: "test-region".to_string(),
            access_key_id: "test-access-key".to_string(),
            access_key_secret: "test-secret-key".to_string(),
        }
    }

    fn hidden_progress() -> MultiProgress {
        MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
    }

    #[test]
    fn skips_upload_when_config_is_none() -> Result<()> {
        let dir = TempDir::new()?;
        let path = dir.path().join("edits.pmtiles");
        std::fs::write(&path, b"tiles")?;
        upload_file_with_config(
            None,
            &path,
            "edits.pmtiles",
            "application/vnd.pmtiles",
            "upload.tiles",
            &hidden_progress(),
        )?;
        Ok(())
    }

    #[test]
    fn reads_config_from_env_when_s3_endpoint_set() -> Result<()> {
        // The only test touching process env vars -- every other test
        // injects an S3Config directly (see `test_config`), so there's
        // no other test racing on these same variables.
        // SAFETY: no other test in this module reads or writes these.
        unsafe {
            env::set_var("S3_ENDPOINT", "http://example.invalid");
            env::set_var("S3_BUCKET", "env-bucket");
            env::set_var("S3_REGION", "env-region");
            env::set_var("S3_ACCESS_KEY_ID", "env-key-id");
            env::set_var("S3_ACCESS_KEY_SECRET", "env-key-secret");
        }
        let config = S3Config::from_env()?.expect("S3_ENDPOINT is set");
        assert_eq!(config.endpoint, "http://example.invalid");
        assert_eq!(config.bucket, "env-bucket");
        // SAFETY: see above.
        unsafe {
            env::remove_var("S3_ENDPOINT");
            env::remove_var("S3_BUCKET");
            env::remove_var("S3_REGION");
            env::remove_var("S3_ACCESS_KEY_ID");
            env::remove_var("S3_ACCESS_KEY_SECRET");
        }
        assert!(S3Config::from_env()?.is_none());
        Ok(())
    }

    #[test]
    fn put_object_for_small_file() -> Result<()> {
        let mut server = Server::new();
        let config = test_config(&server);

        let mock = server
            .mock("PUT", "/test-bucket/edits.pmtiles")
            .match_header("content-type", "application/vnd.pmtiles")
            .with_status(200)
            .with_header("ETag", "\"abc123\"")
            .create();

        let dir = TempDir::new()?;
        let path = dir.path().join("edits.pmtiles");
        std::fs::write(&path, b"small file, well under the multipart threshold")?;

        upload_file_with_config(
            Some(&config),
            &path,
            "edits.pmtiles",
            "application/vnd.pmtiles",
            "upload.tiles",
            &hidden_progress(),
        )?;

        mock.assert();
        Ok(())
    }

    #[test]
    fn multipart_upload_for_large_file() -> Result<()> {
        let mut server = Server::new();
        let config = test_config(&server);

        let create_mock = server
            .mock("POST", "/test-bucket/conflated.parquet")
            .match_query(mockito::Matcher::Regex("uploads".to_string()))
            .with_status(200)
            .with_header("Content-Type", "application/xml")
            .with_body(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<InitiateMultipartUploadResult>
  <Bucket>test-bucket</Bucket>
  <Key>conflated.parquet</Key>
  <UploadId>test-upload-id</UploadId>
</InitiateMultipartUploadResult>"#,
            )
            .create();
        let upload_part_mock = server
            .mock("PUT", "/test-bucket/conflated.parquet")
            .match_query(mockito::Matcher::AllOf(vec![
                // Matches both parts (partNumber=1 and partNumber=2 --
                // one just over MULTIPART_THRESHOLD, one final small
                // part), rather than pinning to a specific number.
                mockito::Matcher::Regex("partNumber=\\d+".to_string()),
                mockito::Matcher::UrlEncoded("uploadId".into(), "test-upload-id".into()),
            ]))
            .with_status(200)
            .with_header("ETag", "\"part1etag\"")
            .expect(2) // one part just over MULTIPART_THRESHOLD, one final small part
            .create();
        let complete_mock = server
            .mock("POST", "/test-bucket/conflated.parquet")
            .match_query(mockito::Matcher::UrlEncoded(
                "uploadId".into(),
                "test-upload-id".into(),
            ))
            .with_status(200)
            .with_header("Content-Type", "application/xml")
            .with_body(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<CompleteMultipartUploadResult>
  <Location>http://example.com/test-bucket/conflated.parquet</Location>
  <Bucket>test-bucket</Bucket>
  <Key>conflated.parquet</Key>
  <ETag>"final-etag"</ETag>
</CompleteMultipartUploadResult>"#,
            )
            .create();

        let dir = TempDir::new()?;
        let path = dir.path().join("conflated.parquet");
        let body = vec![b'x'; MULTIPART_THRESHOLD as usize + 1024];
        std::fs::write(&path, &body)?;

        upload_file_with_config(
            Some(&config),
            &path,
            "conflated.parquet",
            "application/vnd.apache.parquet",
            "upload.conflated",
            &hidden_progress(),
        )?;

        create_mock.assert();
        upload_part_mock.assert();
        complete_mock.assert();
        Ok(())
    }

    #[test]
    fn logs_upload_key_is_run_id_dot_log() -> Result<()> {
        let mut server = Server::new();
        let config = test_config(&server);

        let format = time::format_description::well_known::Rfc3339;
        let t = UtcDateTime::parse("2026-03-04T15:16:17Z", &format)?;

        let mock = server
            .mock("PUT", "/test-bucket/logs/2026-03-04-15-16-17.log")
            .match_header("content-type", "application/x-ndjson")
            .with_status(200)
            .with_header("ETag", "\"logsetag\"")
            .create();

        let dir = TempDir::new()?;
        std::fs::write(dir.path().join("pipeline.log"), b"{\"level\":\"INFO\"}\n")?;

        let destination = format!("logs/{}.log", run_id(t)?);
        upload_file_with_config(
            Some(&config),
            &dir.path().join("pipeline.log"),
            &destination,
            "application/x-ndjson",
            "upload.logs",
            &hidden_progress(),
        )?;

        mock.assert();
        Ok(())
    }

    #[test]
    fn run_id_formats_without_colons() -> Result<()> {
        let format = time::format_description::well_known::Rfc3339;
        let t = UtcDateTime::parse("2026-03-04T15:16:17Z", &format)?;
        assert_eq!(run_id(t)?, "2026-03-04-15-16-17");
        Ok(())
    }
}
