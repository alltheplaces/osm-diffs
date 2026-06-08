# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Shell script to generate a CycloneDX Software Bill of Materials (SBOM)
# for the statically compiled tippecanoe binary, which we ship in our
# OCI container image. This script gets invoked during our container build,
# which automatically runs on GitHub infrastructure after a git tag
# for a fresh release has been pushed.

ALPINE_VERSION=$(grep "^VERSION_ID=" /etc/os-release | cut -d '"' -f 2)
APK_VERSION=$(apk --version             | sed -E 's/.* ([0-9]+\.[0-9]+(\.[0-9]+(-r[0-9]+)?)?).*/\1/')
JQ_VERSION=$(jq --version               | sed -n 's/jq-//p')
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
  "metadata": {
    "timestamp": "${BUILD_TIMESTAMP}",
    "lifecycles": [{"phase": "build"}],
    "authors": [
      {
        "name": "Sascha Brawer",
        "email": "sascha@brawer.ch"
      }
    ],
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
    "tools": {
      "components": [
        {
          "type": "operating-system",
          "name": "Alpine Linux",
          "version": "${ALPINE_VERSION}",
          "bom-ref": "alpine-${ALPINE_VERSION}",
          "description": "Operating system on which this SBOM was built",
          "supplier": {
            "name": "Alpine Linux",
            "url": ["https://alpinelinux.org"]
          }
        },
        {
          "type": "application",
          "name": "apk",
          "version": "${APK_VERSION}",
          "bom-ref": "apk-${APK_VERSION}",
          "description": "Package versions extracted via apk info",
          "supplier": {
            "name": "Alpine Linux",
            "url": ["https://alpinelinux.org"]
          }
        },
        {
          "type": "application",
          "name": "jq",
          "version": "${JQ_VERSION}",
          "bom-ref": "jq-${JQ_VERSION}",
          "description": "Supplemental information injected with jq",
          "supplier": {
            "name": "Alpine Linux",
            "url": ["https://alpinelinux.org"]
          }
	}
      ]
    }
  },
  "components": [
    {
      "type": "library",
      "name": "musl",
      "version": "${MUSL_VERSION}",
      "bom-ref": "musl-${MUSL_VERSION}",
      "purl": "pkg:apk/alpine/musl@${MUSL_VERSION}?arch=${ARCH}",
      "supplier": {
        "name": "Alpine Linux",
        "url": ["https://alpinelinux.org"]
      },
      "licenses": [{ "license": { "id": "MIT" } }],
      "evidence": {
        "identity": [
          {
            "field": "version",
            "confidence": 1,
            "concludedValue": "${MUSL_VERSION}",
            "methods": [{ "technique": "manifest-analysis", "confidence": 1, "value": "apk info musl" }],
            "tools": ["apk-${APK_VERSION}"]
          }
        ]
      }
    },
    {
      "type": "library",
      "name": "sqlite",
      "version": "${SQLITE_VERSION}",
      "bom-ref": "sqlite-${SQLITE_VERSION}",
      "purl": "pkg:apk/alpine/sqlite@${SQLITE_VERSION}?arch=${ARCH}",
      "supplier": {
        "name": "Alpine Linux",
        "url": ["https://alpinelinux.org"]
      },
      "licenses": [{ "license": { "id": "blessing" } }],
      "evidence": {
        "identity": [
          {
            "field": "version",
            "confidence": 1,
            "concludedValue": "${SQLITE_VERSION}",
            "methods": [{ "technique": "manifest-analysis", "confidence": 1, "value": "apk info sqlite-static" }],
            "tools": ["apk-${APK_VERSION}"]
          }
        ]
      }
    },
    {
      "type": "library",
      "name": "zlib",
      "version": "${ZLIB_VERSION}",
      "bom-ref": "zlib-${ZLIB_VERSION}",
      "purl": "pkg:apk/alpine/zlib@${ZLIB_VERSION}?arch=${ARCH}",
      "supplier": {
        "name": "Alpine Linux",
        "url": ["https://alpinelinux.org"]
      },
      "licenses": [{ "license": { "id": "Zlib" } }],
      "evidence": {
        "identity": [
          {
            "field": "version",
            "confidence": 1,
            "concludedValue": "${ZLIB_VERSION}",
            "methods": [{ "technique": "manifest-analysis", "confidence": 1, "value": "apk info zlib-static" }],
            "tools": ["apk-${APK_VERSION}"]
          }
        ]
      }
    }
  ],
  "dependencies": [
    {
      "ref": "tippecanoe-${TIPPECANOE_VERSION}",
      "dependsOn": [
        "musl-${MUSL_VERSION}",
        "sqlite-${SQLITE_VERSION}",
        "zlib-${ZLIB_VERSION}"
      ]
    }
  ]
}
EOF
