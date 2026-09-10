//! Uploads pipeline outputs to S3-compatible storage.
//!
//! There are two independent destinations, each with its own five-var
//! credential set (see [`Bucket`] and [`S3Config`]):
//!
//! - **`PUBLIC_S3_*`** -- the public downloads bucket, fronted by a CDN:
//!   `conflated.parquet` and the PMTiles archives. Intended for a
//!   Bunny.net S3 instance so objects replicate to the edge.
//! - **`INTERNAL_S3_*`** -- in-datacenter storage that never needs CDN
//!   replication: `pipeline.log`, and any future intermediates. Intended
//!   for something like Hetzner Object Storage.
//!
//! Every upload goes through [upload_file], which decides between a
//! single PUT and a multi-part upload based on the file's size (see
//! [MULTIPART_THRESHOLD]) -- multi-part's own per-part minimum makes it
//! pure overhead for something as small as a log file, but it's still
//! the right choice for larger files like tiles or `conflated.parquet`.
//!
//! Each destination's uploads are skipped (not an error) if its
//! `*_ENDPOINT` isn't set -- the established way to disable uploads for a
//! local/dev run, now per-bucket.

use crate::make_download_bar;
use crate::pipeline::datapackage::{self, PublishedFile};
use crate::pipeline::provenance;
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

/// Which S3 bucket an upload targets. See the module doc for the split.
#[derive(Clone, Copy, Debug)]
enum Bucket {
    /// The CDN-fronted public downloads bucket (`PUBLIC_S3_*`).
    Public,
    /// In-datacenter storage, no CDN replication (`INTERNAL_S3_*`).
    Internal,
}

impl Bucket {
    /// Environment-variable prefix for this bucket's five connection
    /// settings: `<prefix>_ENDPOINT` / `_BUCKET` / `_REGION` /
    /// `_ACCESS_KEY_ID` / `_ACCESS_KEY_SECRET`.
    fn env_prefix(self) -> &'static str {
        match self {
            Bucket::Public => "PUBLIC_S3",
            Bucket::Internal => "INTERNAL_S3",
        }
    }
}

/// S3 connection details for one [`Bucket`], read once from the
/// environment and reused for every upload to it in a pipeline run.
///
/// All five come from environment variables, none of them CLI flags
/// (`osm-diffs run --help` won't mention them) -- they're ambient
/// deployment config, the kind you'd set once for wherever this runs on
/// a schedule, not something to pass per invocation. For prefix
/// `PUBLIC_S3` (likewise `INTERNAL_S3`):
///
/// - `PUBLIC_S3_ENDPOINT` -- the S3-compatible service's base URL (e.g.
///   `https://s3.amazonaws.com`, or a MinIO/other provider's own URL).
///   Also that bucket's on/off switch: if this is unset, every upload to
///   that bucket is skipped entirely (see [S3Config::from_env]).
/// - `PUBLIC_S3_BUCKET` -- the bucket every upload to that destination
///   writes to, distinguished by key.
/// - `PUBLIC_S3_REGION` -- passed to the S3 client as-is; some
///   S3-compatible services ignore it, but the client still requires a
///   value.
/// - `PUBLIC_S3_ACCESS_KEY_ID` / `PUBLIC_S3_ACCESS_KEY_SECRET` -- static
///   credentials for that bucket.
struct S3Config {
    endpoint: String,
    bucket: String,
    region: String,
    access_key_id: String,
    access_key_secret: String,
}

impl S3Config {
    /// Reads `<prefix>_ENDPOINT`/`_BUCKET`/`_REGION`/`_ACCESS_KEY_ID`/
    /// `_ACCESS_KEY_SECRET` for `bucket` from the environment. Returns
    /// `Ok(None)` (not an error) if `<prefix>_ENDPOINT` specifically is
    /// unset -- that's how that bucket's uploads are deliberately
    /// disabled. Once `<prefix>_ENDPOINT` *is* set, every other variable
    /// missing is a real configuration error.
    fn from_env(bucket: Bucket) -> Result<Option<S3Config>> {
        let prefix = bucket.env_prefix();
        let Some(endpoint) = env::var(format!("{prefix}_ENDPOINT")).ok() else {
            return Ok(None);
        };
        Ok(Some(S3Config {
            endpoint,
            bucket: env_var(&format!("{prefix}_BUCKET"))?,
            region: env_var(&format!("{prefix}_REGION"))?,
            access_key_id: env_var(&format!("{prefix}_ACCESS_KEY_ID"))?,
            access_key_secret: env_var(&format!("{prefix}_ACCESS_KEY_SECRET"))?,
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

/// Uploads `path` to `destination` in `bucket`. Skips entirely (logging
/// why) if that bucket's `*_ENDPOINT` isn't set. Chooses a single PUT or
/// a multi-part upload based on `path`'s size.
fn upload_file(
    bucket: Bucket,
    path: &Path,
    destination: &str,
    content_type: &str,
    progress_label: &str,
    progress: &MultiProgress,
) -> Result<()> {
    let Some(config) = S3Config::from_env(bucket)? else {
        log::warn!(
            "{}_ENDPOINT not set, skipping upload of {destination}",
            bucket.env_prefix()
        );
        return Ok(());
    };
    upload_file_with_config(
        Some(&config),
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
        log::warn!("no S3 config, skipping upload of {destination}");
        return Ok(());
    };
    let client = config.client()?;

    // Only `data/datapackage.json` is meant to be overwritten (see
    // docs/PRODUCTION.md). Every other key carries a content hash, so an
    // identical rebuild re-PUTs identical bytes -- harmless -- but a key
    // that already exists with *different* bytes means a second run this
    // day published something else under the same name, and the CDN will
    // serve stale bytes until the long TTL expires. Warn, don't fail: a
    // mid-run retry legitimately re-uploads its own partial objects.
    if destination != "data/datapackage.json"
        && let Ok(existing) = client.objects().head(&config.bucket, destination).send()
    {
        log::warn!(
            s3_url = format!("s3://{}/{}", config.bucket, destination),
            existing_etag = existing.etag.as_deref();
            "upload_file: object already exists, replacing it -- if this isn't a retry of \
             today's run, prune this day's data/ objects and re-run"
        );
    }

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

/// Canonical host for the absolute URLs the standalone BOM records
/// (`externalReferences[].url`). A staging hostname during the CDN
/// migration (see `docs/PRODUCTION.md`); renamed at cutover. The
/// manifest itself carries only relative `resources[].path`, so it has
/// no hostname to rename.
const PUBLIC_HOST: &str = "https://osmdiffs.dandelis.ch";

/// A file `publish` hashes, names `data/<stem>-<date>-<hash8>.<ext>`,
/// uploads to the public bucket, and returns as a [`PublishedFile`] for
/// the manifest. All the per-artifact constants in one place.
struct Publish {
    /// Frictionless resource name.
    name: &'static str,
    /// Filename stem before `-<date>-<hash8>`.
    stem: &'static str,
    ext: &'static str,
    title: &'static str,
    description: &'static str,
    /// Frictionless resource `type`. `"file"` for everything here: the
    /// GeoParquet's WKB geometry columns don't fit a Frictionless Table
    /// Schema, and the PMTiles are opaque binary -- consumers use a
    /// GeoParquet / PMTiles reader, not Frictionless's own table parser.
    frictionless_type: &'static str,
    /// Frictionless `format` (bare, e.g. `"parquet"`).
    format: &'static str,
    /// Media type -- also the S3 `Content-Type`.
    mediatype: &'static str,
    describes: Option<&'static str>,
    /// Also compute SHA-512 (only `conflated.parquet` needs it, for its
    /// standalone BOM).
    sha512: bool,
}

const CONFLATED: Publish = Publish {
    name: "conflated",
    stem: "conflated",
    ext: "parquet",
    title: "Conflated AllThePlaces/OpenStreetMap dataset (GeoParquet)",
    description: "This dated object is immutable; the manifest's version is what advances.",
    frictionless_type: "file",
    format: "parquet",
    mediatype: "application/vnd.apache.parquet",
    describes: None,
    sha512: true,
};

const CONFLATED_TILES: Publish = Publish {
    name: "conflated-tiles",
    stem: "conflated",
    ext: "pmtiles",
    title: "conflated.pmtiles \u{2014} visualization of conflated.parquet",
    description: "Debugging aid for reviewing the matching step. Not a data product; \
        structure may change, or production may stop, without notice.",
    frictionless_type: "file",
    format: "pmtiles",
    mediatype: "application/vnd.pmtiles",
    describes: None,
    sha512: false,
};

const EDITS_TILES: Publish = Publish {
    name: "edits-tiles",
    stem: "edits",
    ext: "pmtiles",
    title: "edits.pmtiles \u{2014} visualization of suggested edits",
    description: "Debugging aid. Not a data product; may change, or stop being produced, \
        without notice.",
    frictionless_type: "file",
    format: "pmtiles",
    mediatype: "application/vnd.pmtiles",
    describes: None,
    sha512: false,
};

/// Hashes `local`, uploads it to `data/<stem>-<date>-<hash8>.<ext>` on
/// the public bucket, and returns its manifest entry.
fn publish(
    spec: &Publish,
    local: &Path,
    date: &str,
    progress: &MultiProgress,
) -> Result<PublishedFile> {
    let digest =
        crate::utils::hash_file(local, progress, &format!("hash.{}", spec.name), spec.sha512)?;
    let basename = format!("{}-{date}-{}.{}", spec.stem, &digest.sha256[..8], spec.ext);
    upload_file(
        Bucket::Public,
        local,
        &format!("data/{basename}"),
        spec.mediatype,
        &format!("upload.{}", spec.name),
        progress,
    )?;
    Ok(PublishedFile {
        name: spec.name,
        path: basename,
        title: spec.title,
        description: spec.description,
        frictionless_type: spec.frictionless_type,
        format: spec.format,
        mediatype: spec.mediatype,
        bytes: digest.bytes,
        sha256: digest.sha256,
        sha512: digest.sha512,
        describes: spec.describes,
    })
}

pub fn upload_tiles(tiles: &Path, date: &str, progress: &MultiProgress) -> Result<PublishedFile> {
    publish(&EDITS_TILES, tiles, date, progress)
}

pub fn upload_conflated(
    conflated: &Path,
    date: &str,
    progress: &MultiProgress,
) -> Result<PublishedFile> {
    publish(&CONFLATED, conflated, date, progress)
}

pub fn upload_conflated_tiles(
    tiles: &Path,
    date: &str,
    progress: &MultiProgress,
) -> Result<PublishedFile> {
    publish(&CONFLATED_TILES, tiles, date, progress)
}

/// Writes the standalone CycloneDX BOM sidecar for `conflated.parquet`
/// -- `data/conflated-<date>-<parquet-hash8>.cdx.json` -- and uploads
/// it. Same document as the copy embedded in the Parquet, plus the
/// finished file's own SHA-256/512 and its published-at URL (see
/// [`provenance::OutputFileRef`]). Paired to the Parquet by hash8, so
/// the two basenames line up.
pub fn upload_conflated_bom(
    workdir: &Path,
    pipeline_run_id: &str,
    conflated: &PublishedFile,
    date: &str,
    progress: &MultiProgress,
) -> Result<PublishedFile> {
    let distribution_url = format!("{PUBLIC_HOST}/data/{}", conflated.path);
    let bom = provenance::build_bom_for_conflated_parquet(
        workdir,
        pipeline_run_id,
        Some(provenance::OutputFileRef {
            sha256: &conflated.sha256,
            sha512: conflated
                .sha512
                .as_deref()
                .context("conflated.parquet's SHA-512 was not computed")?,
            distribution_url: &distribution_url,
        }),
    )
    .context("could not assemble standalone provenance BOM")?;

    let basename = format!("conflated-{date}-{}.cdx.json", &conflated.sha256[..8]);
    let path = workdir.join(&basename);
    write_json_pretty(&path, &bom)?;
    let digest = crate::utils::hash_file(&path, progress, "hash.bom", false)?;
    upload_file(
        Bucket::Public,
        &path,
        &format!("data/{basename}"),
        "application/vnd.cyclonedx+json; version=1.7",
        "upload.conflated-bom",
        progress,
    )?;
    Ok(PublishedFile {
        // "bom", not "sbom": this CycloneDX document describes a *data*
        // file (its `metadata.component.type` is `data`), not a software
        // build.
        name: "bom",
        path: basename,
        title: "CycloneDX 1.7 provenance BOM",
        description: "Provenance for conflated.parquet: pipeline version, the two inputs, \
            and the Parquet's own SHA-256/512.",
        frictionless_type: "json",
        format: "json",
        mediatype: "application/vnd.cyclonedx+json; version=1.7",
        bytes: digest.bytes,
        sha256: digest.sha256,
        sha512: None,
        describes: Some("conflated"),
    })
}

/// Builds `data/datapackage.json` over everything published this run and
/// uploads it -- **last**, so "listed in the manifest" always implies
/// "already uploaded". The one object at a stable key.
pub fn upload_datapackage(
    workdir: &Path,
    anchor: time::UtcDateTime,
    published: &[PublishedFile],
    progress: &MultiProgress,
) -> Result<()> {
    let doc = datapackage::build_datapackage(workdir, anchor, published)?;
    verify_manifest(&doc, published)?;
    let path = workdir.join("datapackage.json");
    write_json_pretty(&path, &doc)?;
    upload_file(
        Bucket::Public,
        &path,
        "data/datapackage.json",
        "application/json",
        "upload.datapackage",
        progress,
    )
}

/// Post-build self-check: every resource the manifest lists must be one
/// this run actually uploaded, with a matching size and hash and a bare
/// relative path. Catches a wiring bug before the manifest goes live.
fn verify_manifest(doc: &serde_json::Value, published: &[PublishedFile]) -> Result<()> {
    let resources = doc["resources"]
        .as_array()
        .context("built manifest has no resources array")?;
    anyhow::ensure!(
        resources.len() == published.len(),
        "manifest lists {} resources but {} were published",
        resources.len(),
        published.len()
    );
    for (r, f) in resources.iter().zip(published) {
        anyhow::ensure!(
            !f.path.contains('/') && !f.path.contains(':'),
            "resource path {:?} is not a bare relative basename",
            f.path
        );
        anyhow::ensure!(
            r["path"] == f.path.as_str(),
            "manifest resource path {:?} != published {:?}",
            r["path"],
            f.path
        );
        anyhow::ensure!(
            r["bytes"] == f.bytes,
            "manifest byte size for {} disagrees with what was uploaded",
            f.path
        );
        anyhow::ensure!(
            r["hash"] == format!("sha256:{}", f.sha256),
            "manifest hash for {} disagrees with what was uploaded",
            f.path
        );
    }
    Ok(())
}

/// Writes `value` as pretty JSON to `path` via a temp file and rename,
/// so a crash mid-write leaves no half-written file for a restart.
fn write_json_pretty(path: &Path, value: &serde_json::Value) -> Result<()> {
    let mut tmp = path.to_path_buf();
    tmp.add_extension("tmp");
    let data = serde_json::to_vec_pretty(value)?;
    std::fs::write(&tmp, &data).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Uploads `workdir`'s `pipeline.log` to `logs/<run-id>.log` on the
/// **internal** bucket -- a log is operational, not a public download.
///
/// `<run-id>` is the slugged `--run_id` (see `pipeline::run_id_slug`)
/// when the scheduler supplied one -- so a restarted job appends to
/// (rather than forks) its run's single log object -- and otherwise the
/// process-start timestamp (`YYYY-MM-DD-HH-MM-SS`), for a local run with
/// no `--run_id`. See [docs/LOGGING.md](../../../docs/LOGGING.md) for the
/// user-facing explanation of this layout.
pub fn upload_logs(
    workdir: &Path,
    pipeline_run_id: &str,
    pipeline_start_time: UtcDateTime,
    progress: &MultiProgress,
) -> Result<()> {
    let log_path = workdir.join("pipeline.log");
    let key_stem = if pipeline_run_id.is_empty() {
        run_id(pipeline_start_time)?
    } else {
        pipeline_run_id.to_string()
    };
    let destination = format!("logs/{key_stem}.log");
    // TODO: We should use an official, IANA-assigned content type for
    // JSON Lines here, but as of August 2026, no consensus has yet been
    // reached on what string to use, so the registration appears to be
    // stalled -- see https://github.com/wardi/jsonlines/issues/19.
    // Tracked in alltheplaces/osm-diffs#684 to check back in August 2027.
    upload_file(
        Bucket::Internal,
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
    fn reads_config_from_env_per_bucket() -> Result<()> {
        // The only test touching process env vars -- every other test
        // injects an S3Config directly (see `test_config`), so there's
        // no other test racing on these same variables.
        // SAFETY: no other test in this module reads or writes these.
        unsafe {
            env::set_var("PUBLIC_S3_ENDPOINT", "http://public.invalid");
            env::set_var("PUBLIC_S3_BUCKET", "public-bucket");
            env::set_var("PUBLIC_S3_REGION", "public-region");
            env::set_var("PUBLIC_S3_ACCESS_KEY_ID", "public-key-id");
            env::set_var("PUBLIC_S3_ACCESS_KEY_SECRET", "public-key-secret");
        }
        let public = S3Config::from_env(Bucket::Public)?.expect("PUBLIC_S3_ENDPOINT is set");
        assert_eq!(public.endpoint, "http://public.invalid");
        assert_eq!(public.bucket, "public-bucket");
        // The two buckets are independent: INTERNAL_S3_* is unset here.
        assert!(S3Config::from_env(Bucket::Internal)?.is_none());

        // SAFETY: see above.
        unsafe {
            env::remove_var("PUBLIC_S3_ENDPOINT");
            env::remove_var("PUBLIC_S3_BUCKET");
            env::remove_var("PUBLIC_S3_REGION");
            env::remove_var("PUBLIC_S3_ACCESS_KEY_ID");
            env::remove_var("PUBLIC_S3_ACCESS_KEY_SECRET");
        }
        assert!(S3Config::from_env(Bucket::Public)?.is_none());
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
