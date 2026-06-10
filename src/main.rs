use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use indicatif::MultiProgress;
use reqwest::Client;
use rustls::version::TLS13;
use rustls::{ClientConfig, RootCertStore};
use std::fs::create_dir;
use webpki_roots::TLS_SERVER_ROOTS;

use diffed_places_pipeline::{
    build_coverage, import_atp, import_osm, render_tiles, suggest_edits, upload_tiles,
};

#[derive(Parser)]
#[command(name = "diffed-places-pipeline")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Run {
        #[arg(short, long, value_name = "workdir")]
        workdir: PathBuf,
    },
}

fn main() -> Result<()> {
    let args = Cli::parse();
    env_logger::init();
    init_crypto();

    match &args.command {
        Some(Commands::Run { workdir }) => {
            let client = build_client();
            let progress = MultiProgress::new();
            if !workdir.exists() {
                create_dir(workdir)?;
            }

            let atp = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(import_atp(&client, &progress, workdir))?;

            let coverage = build_coverage(&atp, &progress, workdir)?;
            let osm = import_osm(&coverage, &progress, workdir)?;
            let layers = suggest_edits(&coverage, &atp, &osm, &progress, workdir)?;
            let tiles = render_tiles(&layers, &progress, workdir)?;
            upload_tiles(&tiles, &progress)?;

            Ok(())
        }
        None => Err(anyhow!("no subcommand given")),
    }
}

fn init_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn build_client() -> reqwest::Client {
    let mut root_store = RootCertStore::empty();
    root_store.extend(TLS_SERVER_ROOTS.iter().cloned());

    // Install the crypto provider as process-wide default,
    // which makes librqbit use it as well for BitTorrent.
    let provider = rustls::crypto::aws_lc_rs::default_provider();

    let config = ClientConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&TLS13])
        .unwrap()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Client::builder()
        .use_preconfigured_tls(config)
        .build()
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    static INIT_CRYPTO: OnceLock<()> = OnceLock::new();

    fn init_crypto_once() {
        INIT_CRYPTO.get_or_init(|| {
            init_crypto();
        });
    }

    #[test]
    fn test_build_client_constructs_successfully() {
        init_crypto_once();

        // Verifies the TLS config and client build without panicking.
        // reqwest::Client doesn't expose internals that we could test
        // without sending traffic to external services over the network.
        let _ = build_client();
    }

    #[tokio::test]
    #[ignore = "requires network"]
    async fn test_client_rejects_tls12() {
        init_crypto_once();

        let client = build_client();
        // badssl.com provides test endpoints for various TLS scenarios
        let result = client.get("https://tls-v1-2.badssl.com:1012/").send().await;
        assert!(
            result.is_err(),
            "Expected TLS 1.2 connection to be rejected"
        );
    }

    #[tokio::test]
    #[ignore = "requires network"]
    async fn test_client_accepts_tls13() {
        init_crypto_once();

        let client = build_client();
        let result = client.get("https://tls13.akamai.io/").send().await;
        assert!(
            result.is_ok(),
            "Expected TLS 1.3 connection to succeed: {:?}",
            result.err()
        );
    }
}
