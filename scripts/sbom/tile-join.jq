# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Build a CycloneDX 1.7 SBOM fragment for the statically compiled
# tile-join binary that we ship alongside osm-diffs and tippecanoe in
# the OCI container image -- used by pipeline::tiles::join_tiles to
# merge conflated.pmtiles' overview and detail passes (see
# pipeline::conflated_tiles' module doc comment for why that split
# exists).
#
# tile-join is a sibling binary built from the exact same
# felt/tippecanoe source tree, at the exact same pinned commit, as
# tippecanoe itself (see tippecanoe.jq and Containerfile) -- same
# version, same license, same static-link treatment against
# musl/sqlite/zlib. This fragment is deliberately near-identical to
# tippecanoe.jq's; the one substantive difference is the purl's
# `#tile-join` subpath, identifying which binary within that source
# tree this component describes.
#
# Invoked as `jq -n -f tile-join.jq` (no stdin input; the whole
# document is built from the arguments below).
#
# Arguments (all required, passed with --arg): see tippecanoe.jq --
# identical argument list, describing the identical build environment
# both binaries were compiled in.

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
      name: "tile-join",
      version: $TIPPECANOE_VERSION,
      "bom-ref": ("tile-join-" + $TIPPECANOE_VERSION),
      purl: ("pkg:github/felt/tippecanoe@" + $TIPPECANOE_VERSION + "#tile-join"),
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
      # See tippecanoe.jq's own comment on this field: SQLite isn't
      # SPDX-licensed, so this uses the same "declared" Public Domain
      # pattern, not a `license.id`.
      licenses: [{
        license: {
          name: "Public Domain",
          acknowledgement: "declared"
        }
      }],
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
    ref: ("tile-join-" + $TIPPECANOE_VERSION),
    dependsOn: [
      ("musl-" + $MUSL_VERSION),
      ("sqlite-" + $SQLITE_VERSION),
      ("zlib-" + $ZLIB_VERSION)
    ]
  }]
}
