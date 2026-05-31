# Build a Software Bill Of Materials (SBOM) in CycloneDX 1.6 JSON format.
#
# As of March 2026, the "cargo cyclonedx" tool cannot auto-detect what
# TLS version, crypto algorithms and root certificates we're using,
# so we embed a manually crafted CBOM into the auto-generated SBOM.

SBOM_IN="diffed-places-pipeline.cdx.json"
CBOM_PATCH="sbom/cbom.cdx.json"

cargo cyclonedx --no-build-deps --format json --spec-version=1.5

# Extract exact resolved version from Cargo.lock via cargo metadata
WEBPKI_VERSION=$(cargo metadata --format-version 1 --locked |
  jq -r '.packages[] | select(.name == "webpki-roots") | .version')

jq \
  --slurpfile cbom "$CBOM_PATCH" \
  --arg webpki_version "$WEBPKI_VERSION" \
  '
    .specVersion = "1.6" |
    .metadata.supplier = {
      "name": "Diffed Places Project",
      "url": ["https://github.com/diffed-places/"]
    } |
    .components += ($cbom[0].components | map(
      if .name == "Mozilla Root CA Bundle" then
        .version = $webpki_version |
        .purl = "pkg:cargo/webpki-roots@\($webpki_version)" |
        .externalReferences[0].url = "https://crates.io/crates/webpki-roots/\($webpki_version)"
      else . end
    )) |
    .dependencies += ($cbom[0].dependencies // [])
  ' \
  "$SBOM_IN"
