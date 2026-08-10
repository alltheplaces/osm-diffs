use crate::make_download_bar;
use anyhow::{Context, Ok, Result, anyhow};
use futures_util::StreamExt;
use indicatif::MultiProgress;
use reqwest::Client;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::{fs::File, io::AsyncWriteExt};

pub const ATP_RUN_HISTORY_URL: &str = "https://data.alltheplaces.xyz/runs/history.json";

pub async fn fetch_atp(
    url: &str,
    client: &Client,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<PathBuf> {
    let out_path: PathBuf = workdir.join("alltheplaces.zip");
    if out_path.exists() {
        return Ok(out_path);
    }

    let tmp_path = workdir.join("alltheplaces.zip.tmp");
    let meta_json_path = workdir.join("alltheplaces.meta.json");
    download_atp(url, client, progress, &tmp_path, &meta_json_path).await?;
    std::fs::rename(&tmp_path, &out_path)?; // atomic file system operation
    Ok(out_path)
}

async fn download_atp(
    url: &str,
    client: &Client,
    progress: &MultiProgress,
    dest: &Path,
    meta_json_dest: &Path,
) -> Result<()> {
    let mut file = File::create(dest)
        .await
        .with_context(|| format!("Failed to create file {}", dest.display()))?;

    let (run_id, url) = fetch_latest_build(client, url).await?;
    let response = client
        .get(&url)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .with_context(|| format!("Failed to GET {url}"))?
        .error_for_status()
        .with_context(|| format!("Server returned error for {url}"))?;
    let content_length = response.content_length();
    let mut stream = response.bytes_stream();
    let bar = make_download_bar(progress, "atp.fetch     ", content_length);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Error reading chunk from response stream")?;
        file.write_all(&chunk)
            .await
            .context("Failed to write chunk to disk")?;
        bar.inc(chunk.len() as u64);
    }
    file.flush()
        .await
        .with_context(|| format!("Failed to flush {}", dest.display()))?;
    write_meta_json(&run_id, &url, meta_json_dest).await?;
    bar.finish();
    Ok(())
}

async fn fetch_latest_build(client: &Client, url: &str) -> Result<(String, String)> {
    let response = client
        .get(url)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .with_context(|| format!("Failed to GET {url}"))?
        .error_for_status()
        .with_context(|| format!("Server returned error for {url}"))?;
    let json: serde_json::Value = response.json().await?;
    let last_entry = json
        .as_array()
        .and_then(|arr| arr.last())
        .ok_or(anyhow!("Could not find last element of {}", url))?;
    let run_id = last_entry
        .get("run_id")
        .and_then(|v| v.as_str())
        .ok_or(anyhow!(
            "Could not find 'run_id' in last element of {}",
            url
        ))?;
    let build_url = last_entry
        .get("output_url")
        .and_then(|v| v.as_str())
        .ok_or(anyhow!(
            "Could not find 'output_url' in last element of {}",
            url
        ))?;
    Ok((String::from(run_id), String::from(build_url)))
}

async fn write_meta_json(run_id: &str, url: &str, dest: &Path) -> Result<()> {
    let mut file = File::create(dest)
        .await
        .with_context(|| format!("Failed to create {}", dest.display()))?;
    let data = json!({
        "run_id": run_id,
        "url": url,
    });
    file.write_all(data.to_string().as_bytes()).await?;
    file.flush()
        .await
        .with_context(|| format!("Failed to flush {}", dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indicatif::ProgressDrawTarget;
    use mockito::Server;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_fetch_atp() -> Result<()> {
        let mut server = Server::new_async().await;
        let server_url = server.url();
        let mock_history_payload = json!([
            {"run_id": "2026-03-04-15-16-17", "output_url": format!("{}/atp_data", server_url)}
        ]);
        let mock_history_url = format!("{}/history", server_url);
        let mock_history = server
            .mock("GET", "/history")
            .with_status(200)
            .with_header("Content-Type", "application/json")
            .with_body(mock_history_payload.to_string().as_bytes())
            .create_async()
            .await;
        let mock_atp_data = server
            .mock("GET", "/atp_data")
            .with_status(200)
            .with_header("Content-Type", "application/zip")
            .with_body(b"data")
            .create_async()
            .await;
        let client = test_client(&server);
        let progress = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
        let workdir = TempDir::new()?;
        let path = fetch_atp(&mock_history_url, &client, &progress, workdir.path()).await?;
        mock_history.assert_async().await;
        mock_atp_data.assert_async().await;

        assert!(path.exists());
        assert_eq!(tokio::fs::read(&path).await?, b"data");

        let meta_json_path = workdir.path().join("alltheplaces.meta.json");
        let meta_json_str = tokio::fs::read_to_string(meta_json_path).await?;
        let meta_json: serde_json::Value = serde_json::from_str(&meta_json_str)?;
        assert_eq!(
            meta_json,
            json!({
            "run_id": "2026-03-04-15-16-17",
            "url": format!("{}/atp_data", server_url)
                })
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_latest_build() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[
                    {"run_id": "2017-12-14-02-01-04", "output_url": "https://example.org/first.zip"},
                    {"run_id": "2026-03-18-13-32-34", "output_url": "https://example.org/last.zip"}
                ]"#,
            )
            .create_async()
            .await;
        let client = test_client(&server);
        let result = fetch_latest_build(&client, &server.url()).await.unwrap();
        assert_eq!(result.0, "2026-03-18-13-32-34");
        assert_eq!(result.1, "https://example.org/last.zip");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_fetch_latest_build_missing_output_url() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"[{"other_field": "value"}]"#)
            .create_async()
            .await;

        let client = test_client(&server);
        let result = fetch_latest_build(&client, &server.url()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_write_meta() {}

    fn test_client(_server: &Server) -> Client {
        Client::builder()
            // Mockito runs on 127.0.0.1; no proxy needed.
            .no_proxy()
            .build()
            .expect("failed to build test client")
    }
}
