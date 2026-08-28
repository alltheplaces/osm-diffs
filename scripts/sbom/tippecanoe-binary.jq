# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Build a CycloneDX 1.7 SBOM fragment for one statically compiled
# binary from the felt/tippecanoe build that we ship alongside
# osm-diffs in the OCI container image -- tippecanoe itself, or its
# sibling tile-join (used by pipeline::tiles::join_tiles to merge
# conflated.pmtiles' overview and detail passes; see
# pipeline::conflated_tiles' module doc comment for why that split
# exists). Both binaries come from the exact same source tree, at the
# exact same pinned commit, with the exact same static-link treatment
# against musl/sqlite/zlib -- this template is instantiated once per
# binary (see generate-sbom.sh's build_binary_fragment) rather than
# duplicated, since everything but the component's name and purl is
# identical either way.
#
# Invoked as `jq -n -f tippecanoe-binary.jq` (no stdin input; the whole
# document is built from the arguments below).
#
# Arguments (all required, passed with --arg):
#   NAME               component name: "tippecanoe" or "tile-join"
#   PURL_SUFFIX        appended to the shared package-url, after the
#                       version -- "" for tippecanoe itself, "#tile-join"
#                       to identify that binary within the same source tree
#   ARCH               target architecture (amd64 | aarch64)
#   TIPPECANOE_VERSION version (git tag) of the tippecanoe build -- also
#                       tile-join's own version, since it's the same build
#   ALPINE_VERSION     Alpine Linux version of the build environment
#                       ("dev-unknown" outside of Alpine)
#   APK_VERSION        version of the apk package manager
#   JQ_VERSION         version of jq used to build this SBOM
#   MUSL_VERSION       version of the musl libc this binary was linked against
#   SQLITE_VERSION     version of the sqlite library this binary was linked against
#   ZLIB_VERSION       version of the zlib library this binary was linked against
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
      name: $NAME,
      version: $TIPPECANOE_VERSION,
      "bom-ref": ($NAME + "-" + $TIPPECANOE_VERSION),
      purl: ("pkg:github/felt/tippecanoe@" + $TIPPECANOE_VERSION + $PURL_SUFFIX),
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
      # SQLite isn't SPDX-licensed; upstream dedicates it to the public
      # domain and informally calls that dedication a "blessing" (see
      # https://www.sqlite.org/copyright.html). "blessing" itself isn't a
      # valid SPDX license id, so it doesn't belong in a `license.id`
      # field. Follow the same pattern used for osm-testdata-grid in
      # pipeline.jq: a plain, declared "Public Domain" license name.
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
    ref: ($NAME + "-" + $TIPPECANOE_VERSION),
    dependsOn: [
      ("musl-" + $MUSL_VERSION),
      ("sqlite-" + $SQLITE_VERSION),
      ("zlib-" + $ZLIB_VERSION)
    ]
  }]
}
