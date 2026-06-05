# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Shell script to generate a CycloneDX Software Bill of Materials (SBOM)
# for the statically compiled tippecanoe binary, which we ship in our
# OCI container image. This script gets invoked during our container build,
# which automatically runs on GitHub infrastructure after a git tag
# for a fresh release has been pushed.

MUSL_VERSION=$(apk info musl            | head -1 | sed 's/musl-//;s/ .*//')
SQLITE_VERSION=$(apk info sqlite-static | head -1 | sed 's/sqlite-static-//;s/ .*//')
ZLIB_VERSION=$(apk info zlib-static     | head -1 | sed 's/zlib-static-//;s/ .*//')

UUID=$(cat /proc/sys/kernel/random/uuid)

ARCH=$(uname -m)
case "$ARCH" in
    x86_64) ARCH="amd64" ;;
    arm64)  ARCH="aarch64" ;;
esac

cat << EOF | jq .
{
  "bomFormat": "CycloneDX",
  "specVersion": "1.7",
  "serialNumber": "urn:uuid:${UUID}",
  "version": 1,
  "lifecycles": [{phase: "build"}],
  "metadata": {
    "timestamp": "${BUILD_TIMESTAMP}",
    "supplier": {
      "name": "Diffed Places",
      "url": ["https://github.com/diffed-places"]
    },
    "component": {
      "type": "application",
      "name": "tippecanoe",
      "version": "${TIPPECANOE_VERSION}",
      "bom-ref": "tippecanoe-${TIPPECANOE_VERSION}",
      "purl": "pkg:github/felt/tippecanoe@${TIPPECANOE_VERSION}",
      "supplier": {
        "name": "Felt",
        "url": ["https://github.com/felt/tippecanoe"]
      },
      "licenses": [{ "license": { "id": "BSD-2-Clause" } }]
    },
    "tools": [
      {
        "type": "manual",
        "name": "apk info",
        "description": "Package versions extracted via apk info"
      }
    ]
  },
  "citations": [
    {
      "id": "cite-apk-zlib",
      "source": {
        "name": "Alpine Package Manager",
        "url": "https://pkgs.alpinelinux.org"
      },
      "attributedTo": "apk info zlib-static",
      "process": "Build-time query during container builder stage"
    },
    {
      "id": "cite-apk-sqlite",
      "source": {
        "name": "Alpine Package Manager",
        "url": "https://pkgs.alpinelinux.org"
      },
      "attributedTo": "apk info sqlite-static",
      "process": "Build-time query during container builder stage"
    },
    {
      "id": "cite-apk-musl",
      "source": {
        "name": "Alpine Package Manager",
        "url": "https://pkgs.alpinelinux.org"
      },
      "attributedTo": "apk info musl",
      "process": "Build-time query during container builder stage"
    }
  ],
  "components": [
    {
      "type": "library",
      "name": "musl",
      "version": "${MUSL_VERSION}",
      "purl": "pkg:apk/alpine/musl@${MUSL_VERSION}?arch=${ARCH}",
      "supplier": {
        "name": "Alpine Linux",
        "url": ["https://alpinelinux.org"]
      },
      "licenses": [{ "license": { "id": "MIT" } }],
      "evidence": { "identity": [{ "field": "version", "confidence": 1, "methods": [{ "technique": "instrumentation", "confidence": 1, "value": "apk info musl" }] }] }
    },
    {
      "type": "library",
      "name": "sqlite",
      "version": "${SQLITE_VERSION}",
      "purl": "pkg:apk/alpine/sqlite@${SQLITE_VERSION}?arch=${ARCH}",
      "supplier": {
        "name": "Alpine Linux",
        "url": ["https://alpinelinux.org"]
      },      "licenses": [{ "license": { "id": "blessing" } }],
      "evidence": { "identity": [{ "field": "version", "confidence": 1, "methods": [{ "technique": "instrumentation", "confidence": 1, "value": "apk info sqlite-static" }] }] }
    },
    {
      "type": "library",
      "name": "zlib",
      "version": "${ZLIB_VERSION}",
      "purl": "pkg:apk/alpine/zlib@${ZLIB_VERSION}?arch=${ARCH}",
      "supplier": {
        "name": "Alpine Linux",
        "url": ["https://alpinelinux.org"]
      },
      "licenses": [{ "license": { "id": "Zlib" } }],
      "evidence": { "identity": [{ "field": "version", "confidence": 1, "methods": [{ "technique": "instrumentation", "confidence": 1, "value": "apk info zlib-static" }] }] }
    }
  ]
}
EOF
