use crate::make_download_bar;
use anyhow::{Context, Result};
use indicatif::MultiProgress;
use std::{env, fs::File, io::Read, path::Path};

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
    ) -> Result<Upload<'a>> {
        let upload = client
            .objects()
            .create_multipart_upload(bucket, destination)
            .content_type("application/vnd.pmtiles")?
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
    fn finish(&mut self) -> Result<()> {
        let _result = self
            .client
            .objects()
            .complete_multipart_upload(self.bucket, self.destination, &self.upload_id)
            .parts(self.parts.clone())?
            .send()?;
        self.parts.clear();
        Ok(())
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

pub fn upload_tiles(tiles: &Path, progress: &MultiProgress) -> Result<()> {
    let Some(endpoint) = env::var("S3_ENDPOINT").ok() else {
        eprintln!("S3_ENDPOINT not set, skipping upload");
        return Ok(());
    };

    let mut file = File::open(tiles).with_context(|| format!("cannot open {tiles:?}"))?;
    let num_bytes = file.metadata()?.len();
    let progress_bar = make_download_bar(progress, "upload.tiles", Some(num_bytes));

    let bucket = env_var("S3_BUCKET")?;
    let destination = "edits.pmtiles";
    let auth = s3::Auth::Static(s3::Credentials::new(
        env_var("S3_ACCESS_KEY_ID")?,
        env_var("S3_ACCESS_KEY_SECRET")?,
    )?);
    let addressing = if endpoint.contains("amazonaws.com") {
        s3::AddressingStyle::Auto
    } else {
        s3::AddressingStyle::Path
    };

    let client = s3::BlockingClient::builder(&endpoint)?
        .region(&env_var("S3_REGION")?)
        .auth(auth)
        .addressing_style(addressing)
        .build()?;

    let mut upload = Upload::create(&client, &bucket, destination)?;

    // Some S3 implementations require at least 5 MiB for all parts
    // in a multi-part upload, except the last part.
    const PART_SIZE: usize = 8 * 1024 * 1024;
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

    upload.finish()?;
    progress_bar.finish();

    Ok(())
}

fn env_var(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("Missing environment variable: {name}"))
}
