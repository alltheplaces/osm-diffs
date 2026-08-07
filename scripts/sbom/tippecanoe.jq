# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Build a CycloneDX 1.7 SBOM fragment for the statically compiled
# tippecanoe binary that we ship alongside osm-diffs in the OCI
# container image.
#
# Invoked as `jq -n -f tippecanoe.jq` (no stdin input; the whole
# document is built from the arguments below).
#
# Arguments (all required, passed with --arg):
#   ARCH               target architecture (amd64 | aarch64)
#   TIPPECANOE_VERSION version (git tag) of the tippecanoe build
#   ALPINE_VERSION     Alpine Linux version of the build environment
#                       ("dev-unknown" outside of Alpine)
#   APK_VERSION        version of the apk package manager
#   JQ_VERSION         version of jq used to build this SBOM
#   MUSL_VERSION       version of the musl libc tippecanoe was linked against
#   SQLITE_VERSION     version of the sqlite library tippecanoe was linked against
#   ZLIB_VERSION       version of the zlib library tippecanoe was linked against
#   DEV_BUILD          "true" if built outside of the real Alpine build
#                       environment (placeholders were used for the apk-derived
#                       values above)

def alpine_supplier: {name: "Alpine Linux", url: ["https://alpinelinux.org"]};

{
  bomFormat: "CycloneDX",
  specVersion: "1.7",
  metadata: {
    lifecycles: [{phase: "build"}],
    authors: [{name: "Sascha Brawer", email: "sascha@brawer.ch"}],
    supplier: {name: "All The Places", url: ["https://github.com/alltheplaces/"]},
    component: {
      type: "application",
      name: "tippecanoe",
      version: $TIPPECANOE_VERSION,
      "bom-ref": ("tippecanoe-" + $TIPPECANOE_VERSION),
      purl: ("pkg:github/felt/tippecanoe@" + $TIPPECANOE_VERSION),
      supplier: {name: "Felt", url: ["https://github.com/felt/tippecanoe"]},
      licenses: [{license: {id: "BSD-2-Clause"}}]
    },
    tools: {
      components: [{
        type: "operating-system",
        name: "Alpine Linux",
        version: $ALPINE_VERSION,
        "bom-ref": ("alpine-" + $ALPINE_VERSION),
        description: "Operating system on which this SBOM was built",
        supplier: alpine_supplier
      }, {
        type: "application",
        name: "apk",
        version: $APK_VERSION,
        "bom-ref": ("apk-" + $APK_VERSION),
        description: "Package versions extracted via apk info",
        supplier: alpine_supplier
      }, {
        type: "application",
        name: "jq",
        version: $JQ_VERSION,
        "bom-ref": ("jq-" + $JQ_VERSION),
        description: "Supplemental information injected with jq",
        supplier: alpine_supplier
      }]
    },
    properties: (if $DEV_BUILD == "true" then
        [{name: "osm-diffs:sbom:devBuild", value: "true"}]
      else
        []
      end)
  },
  components: [
    {
      type: "library",
      name: "musl",
      version: $MUSL_VERSION,
      "bom-ref": ("musl-" + $MUSL_VERSION),
      purl: ("pkg:apk/alpine/musl@" + $MUSL_VERSION + "?arch=" + $ARCH),
      supplier: alpine_supplier,
      licenses: [{license: {id: "MIT"}}],
      evidence: {
        identity: [{
          field: "version",
          confidence: 1,
          concludedValue: $MUSL_VERSION,
          methods: [{technique: "manifest-analysis", confidence: 1, value: "apk info musl"}],
          tools: [("apk-" + $APK_VERSION)]
        }]
      }
    },
    {
      type: "library",
      name: "sqlite",
      version: $SQLITE_VERSION,
      "bom-ref": ("sqlite-" + $SQLITE_VERSION),
      purl: ("pkg:apk/alpine/sqlite@" + $SQLITE_VERSION + "?arch=" + $ARCH),
      supplier: alpine_supplier,
      licenses: [{license: {id: "blessing"}}],
      evidence: {
        identity: [{
          field: "version",
          confidence: 1,
          concludedValue: $SQLITE_VERSION,
          methods: [{technique: "manifest-analysis", confidence: 1, value: "apk info sqlite-static"}],
          tools: [("apk-" + $APK_VERSION)]
        }]
      }
    },
    {
      type: "library",
      name: "zlib",
      version: $ZLIB_VERSION,
      "bom-ref": ("zlib-" + $ZLIB_VERSION),
      purl: ("pkg:apk/alpine/zlib@" + $ZLIB_VERSION + "?arch=" + $ARCH),
      supplier: alpine_supplier,
      licenses: [{license: {id: "Zlib"}}],
      evidence: {
        identity: [{
          field: "version",
          confidence: 1,
          concludedValue: $ZLIB_VERSION,
          methods: [{technique: "manifest-analysis", confidence: 1, value: "apk info zlib-static"}],
          tools: [("apk-" + $APK_VERSION)]
        }]
      }
    }
  ],
  dependencies: [{
    ref: ("tippecanoe-" + $TIPPECANOE_VERSION),
    dependsOn: [
      ("musl-" + $MUSL_VERSION),
      ("sqlite-" + $SQLITE_VERSION),
      ("zlib-" + $ZLIB_VERSION)
    ]
  }]
}
