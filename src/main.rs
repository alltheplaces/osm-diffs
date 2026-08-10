use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use reqwest::Client;
use rustls::version::TLS13;
use rustls::{ClientConfig, RootCertStore};
use webpki_roots::TLS_SERVER_ROOTS;

/// Computes diffs between AllThePlaces and OpenStreetMap data, producing
/// edit suggestions to help keep OpenStreetMap complete and up to date.
#[derive(Parser)]
#[command(name = "diffed-places-pipeline", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the full pipeline: fetch inputs, compute diffs, write output.
    Run {
        /// Directory for input, intermediate, and output files.
        #[arg(short, long, value_name = "workdir")]
        workdir: PathBuf,
    },
}

fn main() -> Result<()> {
    let args = Cli::parse();
    init_crypto();

    match &args.command {
        Some(Commands::Run { workdir }) => {
            let client = build_client();
            osm_diffs::run_pipeline(&client, workdir)
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

    // The SBOM's Cryptographic Bill of Materials (see scripts/sbom/pipeline.jq)
    // asserts which TLS 1.3 cipher suites we support. Unlike the TLS library,
    // backend and version -- which are explicit, easily reviewed lines in
    // build_client() above -- the cipher suite *set* isn't chosen by this
    // codebase at all: it's whatever aws-lc-rs's default crypto provider
    // ships. A routine `cargo update` could silently change that set with no
    // diff in this repo's own source for a reviewer to notice. This test
    // catches that: it reads the live provider's actual TLS 1.3 cipher
    // suites and compares them against the same JSON file the SBOM is
    // generated from, so the two can't drift apart unnoticed.
    #[test]
    fn cbom_cipher_suites_match_sbom_facts() {
        init_crypto_once();

        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let actual: Vec<String> = provider
            .cipher_suites
            .iter()
            .filter(|cs| cs.tls13().is_some())
            .map(|cs| {
                // rustls names these "TLS13_AES_256_GCM_SHA384" etc., not
                // "TLS_AES_256_GCM_SHA384" as registered with IANA and used
                // in the SBOM; normalize the prefix before comparing.
                cs.suite()
                    .as_str()
                    .expect("known cipher suite has a name")
                    .replacen("TLS13_", "TLS_", 1)
            })
            .collect();

        let expected: Vec<String> =
            serde_json::from_str(include_str!("../scripts/sbom/tls-1.3-cipher-suites.json"))
                .expect("scripts/sbom/tls-1.3-cipher-suites.json is valid JSON");

        assert_eq!(
            actual, expected,
            "aws-lc-rs's default TLS 1.3 cipher suites changed -- update \
             scripts/sbom/tls-1.3-cipher-suites.json (and reconsider the \
             SBOM's CBOM section in scripts/sbom/pipeline.jq) if this is \
             expected"
        );
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
