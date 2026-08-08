# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Enrich the raw CycloneDX SBOM produced by `cargo cyclonedx` for the
# osm-diffs binary: fix up the dependency graph, attach build-environment
# and supplier metadata, and add the vendored "data" components (and the
# Cryptographic Bill of Materials entries) that cargo-cyclonedx cannot see
# on its own.
#
# Input (stdin): the raw CycloneDX 1.5 document from `cargo cyclonedx`.
# Output: a CycloneDX 1.7 document, still describing only the osm-diffs
#   application (not the final container -- that's assembled by merge.jq).
#
# Arguments (all required, passed with --arg):
#   ALPINE_VERSION            Alpine Linux version of the build environment
#                             ("dev-unknown" outside of Alpine)
#   ARCH                      target architecture (amd64 | aarch64)
#   AWS_LC_SYS_VERSION        version of the aws-lc-sys crate (from Cargo.lock)
#   CARGO_CYCLONEDX_VERSION   version of the cargo-cyclonedx tool
#   ID_TAGGING_SCHEMA_LICENSE SPDX license id of the vendored id-tagging-schema
#   ID_TAGGING_SCHEMA_PURL    package URL of the vendored id-tagging-schema
#   ID_TAGGING_SCHEMA_VERSION version of the vendored id-tagging-schema
#   JQ_VERSION                version of jq used to build this SBOM
#   OSM_TESTDATA_COMMIT       commit hash of the vendored osm-testdata grid
#   PROTOC_VERSION            version of the Protocol Buffers compiler
#   RUSTC_VERSION             version of the Rust compiler
#   DEV_BUILD                 "true" if built outside of the real Alpine
#                             build environment (placeholders were used)

# Add a crates.io supplier to a component object if it lacks one.
def add_supplier:
  if .supplier == null or .supplier == {} then
    .supplier = {"name": "crates.io", "url": ["https://crates.io"]}
  else
    .
  end;

# Patch bom-ref of main application to read "osm-diffs-1.2.3"
# instead of "path+file:///Users/sascha/src/osm-diffs#osm-diffs".
.metadata.component."bom-ref" as $orig_root_ref |
( .metadata.component.name + "-" + .metadata.component.version ) as $root_ref |
.metadata.component."bom-ref" = $root_ref |

# Locate the root component's entry in the dependency graph by its
# original bom-ref, not by assuming it's .dependencies[0] -- cargo-cyclonedx
# doesn't document any ordering guarantee for `.dependencies`.
(
  [.dependencies[] | select(.ref == $orig_root_ref)] | length
) as $root_matches |
if $root_matches != 1 then
  error("pipeline.jq: expected exactly one dependency entry with ref "
        + $orig_root_ref + ", found " + ($root_matches | tostring))
else . end |
(
  .dependencies | map(.ref == $orig_root_ref) | index(true)
) as $root_idx |
.dependencies[$root_idx].ref = $root_ref |

# Declare the root component's dependency on our two vendored, non-crate
# "data" components, so the dependency graph has a single root instead of
# three (the FOSSA NTIA validator flags orphaned components -- ones no
# other component depends on -- as extra roots).
.dependencies[$root_idx].dependsOn =
  ((.dependencies[$root_idx].dependsOn // []) + ["id-tagging-schema", "osm-testdata-grid"]) |

.bomFormat = "CycloneDX" |
.specVersion = "1.7" |
.metadata.lifecycles = [{phase: "build"}] |
.metadata.authors = [{name: "Sascha Brawer", email: "sascha@brawer.ch"}] |
.metadata.supplier = {
  name: "All The Places",
  url: ["https://github.com/alltheplaces/"]
} |
.metadata.tools = {
  components: [{
      type: "operating-system",
      name: "Alpine Linux",
      version: $ALPINE_VERSION,
      "bom-ref": "sbom-os",
      description: "Operating system on which this SBOM was built",
      supplier: {
        name: "Alpine Linux",
        url: ["https://alpinelinux.org"]
      }
    }, {
      type: "application",
      name: "cargo-cyclonedx",
      "bom-ref": "cargo-cyclonedx",
      version: $CARGO_CYCLONEDX_VERSION,
      purl: "pkg:apk/alpine/cargo-cyclonedx@" + $CARGO_CYCLONEDX_VERSION + "?arch=" + $ARCH,
      supplier: {
        name: "Alpine Linux",
        url: ["https://alpinelinux.org"]
      }
    }, {
      type: "application",
      name: "jq",
      "bom-ref": "jq",
      version: $JQ_VERSION,
      purl: "pkg:apk/alpine/jq@" + $JQ_VERSION + "?arch=" + $ARCH,
      supplier: {
        name: "Alpine Linux",
        url: ["https://alpinelinux.org"]
      }
    }
  ]
} |
.metadata.component.supplier = {name: "All The Places", url: ["https://github.com/alltheplaces/"]} |
.metadata.component.purl = "pkg:github/alltheplaces/osm-diffs@" + .metadata.component.version |
.metadata.component.licenses = [{expression: "MIT"}] |
.components |= [ .[] | add_supplier ] |

# Declare that we only use TLS 1.3, with the AWS BoringSSL fork.
.metadata.component.properties += [
  {name: "cdx:cbom:version",      value: "1.0"},
  {name: "crypto:tls:library",    value: "rustls"},
  {name: "crypto:tls:backend",    value: "aws-lc-rs"},
  {name: "crypto:tls:minVersion", value: "1.3"},
  {name: "crypto:tls:maxVersion", value: "1.3"}
] |
.formulation = [{
    "bom-ref": "build-formulation",
    components: [{
      type: "operating-system",
      name: "Alpine Linux",
      version: $ALPINE_VERSION,
      "bom-ref": "build-os",
      description: "Operating system for building binaries",
      supplier: {
        name: "Alpine Linux",
        url: ["https://alpinelinux.org"]
      }
    }, {
      type: "application",
      "bom-ref": "build-rustc",
      name: "rustc",
      version: $RUSTC_VERSION,
      purl: "pkg:generic/rust-lang/rustc@" + $RUSTC_VERSION,
      description: "Rust compiler",
      supplier: {
        name: "The Rust Project",
        url: ["https://www.rust-lang.org"]
      }
    }, {
      type: "application",
      "bom-ref": "build-protoc",
      name: "protoc",
      version: $PROTOC_VERSION,
      purl: "pkg:apk/alpine/protoc@" + $PROTOC_VERSION + "?arch=" + $ARCH,
      description: "Protocol Buffers compiler",
      supplier: {
        name: "Alpine Linux",
        url: ["https://alpinelinux.org"]
      }
    }]
}] |
.components += [
  {
    "type": "data",
    "bom-ref": "id-tagging-schema",
    "name": "id-tagging-schema",
    "description": "OpenStreetMap tagging schema",
    "version": $ID_TAGGING_SCHEMA_VERSION,
    "purl": $ID_TAGGING_SCHEMA_PURL,
    "licenses": [{"license": {"id": $ID_TAGGING_SCHEMA_LICENSE}}],
    "externalReferences": [{
        "type": "vcs",
        "url": "https://github.com/openstreetmap/id-tagging-schema"
    }],
    "manufacturer": {
      "name": "iD Tagging Schema project (OpenStreetMap)",
      "url": [ "https://github.com/openstreetmap/id-tagging-schema" ]
    },
    "supplier": {
      "name": "iD Tagging Schema project (OpenStreetMap)",
      "url": [ "https://github.com/openstreetmap/id-tagging-schema" ]
    }
  },
  {
    "type": "data",
    "bom-ref": "osm-testdata-grid",
    "name": "osm-testdata-grid",
    "description": "OSM grid test fixtures, vendored for unit/integration tests only",
    "version": $OSM_TESTDATA_COMMIT,
    "scope": "excluded",
    "purl": "pkg:github/osmcode/osm-testdata@" + $OSM_TESTDATA_COMMIT + "#grid/data",
    "licenses": [{
      "license": {
        "name": "Public Domain",
        "acknowledgement": "declared"
      }
    }],
    "externalReferences": [{
      "type": "vcs",
      "url": "https://github.com/osmcode/osm-testdata/tree/" + $OSM_TESTDATA_COMMIT + "/grid/data"
    }],
    "manufacturer": {
      "name": "osmcode / Jochen Topf",
      "url": ["https://github.com/osmcode/osm-testdata"]
    },
    "supplier": {
      "name": "osmcode / Jochen Topf",
      "url": ["https://github.com/osmcode/osm-testdata"]
    }
  },
  {
    "type": "library",
    "bom-ref": "pkg/aws-lc",
    "name": "aws-lc",
    "author": "AWS Cryptography",
    "purl": "pkg:github/aws/aws-lc@v" + $AWS_LC_SYS_VERSION,
    "version": $AWS_LC_SYS_VERSION,
    "description":"AWS fork of BoringSSL; native crypto primitives beneath aws-lc-rs",
    "supplier": {
      "name": "AWS Cryptography",
      "url": ["https://github.com/aws/aws-lc"]
     },
     "licenses": [{"expression": "Apache-2.0 OR ISC" }],
     "externalReferences":[{
       "type": "certification-report",
       "url": "https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/4816",
       "comment": "FIPS 140-3 Level 1 — CMVP certificate #4816"
     }]
  },
  {
    "type": "cryptographic-asset",
    "bom-ref": "crypto/protocol/tls-1.3",
    "name": "TLS",
    "version": "1.3",
    "cpe": "cpe:2.3:a:ietf:tls:1.3:*:*:*:*:*:*:*",
    "supplier": {
      "name": "IETF",
      "url": ["https://www.rfc-editor.org/rfc/rfc8446"]
    },
    "cryptoProperties": {
      "assetType": "protocol",
      "protocolProperties": {
        "type": "tls",
        "version": "1.3",
        "cipherSuites": [
          {"name":"TLS_AES_256_GCM_SHA384"},
          { "name": "TLS_AES_128_GCM_SHA256" },
          { "name": "TLS_CHACHA20_POLY1305_SHA256" }
        ]
      }
    }
  }
] |
.dependencies += [
  {
    ref: (.components[] | select(.name == "aws-lc-sys") | ."bom-ref"),
    dependsOn: ["pkg/aws-lc"]
  }
] |

(.components[] | select(.name == "rustls") | ."bom-ref") as $rustls_ref |
(.dependencies[] | select(.ref == $rustls_ref)).dependsOn += ["crypto/protocol/tls-1.3"] |

if $DEV_BUILD == "true" then
  .metadata.properties = ((.metadata.properties // []) + [{
    name: "osm-diffs:sbom:devBuild",
    value: "true"
  }])
else
  .
end
