# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Assemble the final, single SBOM for the container image out of the
# osm-diffs (pipeline) and tippecanoe component fragments.
#
# Invoked as `jq -n -f merge.jq` (no stdin input; both fragments are read
# via --slurpfile).
#
# Note: at the point this script runs (inside `Containerfile`, during
# `podman build`), the image digest is not known yet -- it only exists
# once the build has finished. So `metadata.component.version` is left
# unset here; `release.yml` patches it in afterwards with a one-line
# `jq` expression once `podman inspect` has produced the real digest.
# The final artifact this script writes is still complete and valid on
# its own, e.g. for local development or for `test-container.yml`, which
# never publishes a real image and has no digest to patch in.
#
# Arguments (all required, passed with --arg unless noted):
#   serial      SBOM serial number (a "urn:uuid:..." string)
#   image       container image name, e.g. "alltheplaces/osm-diffs"
#   timestamp   build timestamp, RFC 3339
#   pipeline    (--slurpfile) the enriched osm-diffs SBOM fragment, as
#               produced by pipeline.jq
#   tippecanoe  (--slurpfile) the tippecanoe SBOM fragment, as produced
#               by tippecanoe.jq

($pipeline[0].metadata.component)                              as $meta_pipeline        |
($tippecanoe[0].metadata.component)                            as $meta_tippecanoe      |
($meta_pipeline   | (.["bom-ref"] // .name))                   as $ref_pipeline         |
($meta_tippecanoe | (.["bom-ref"] // .name))                   as $ref_tippecanoe       |
($tippecanoe[0].components // [] | map(.["bom-ref"] // .name)) as $deps_of_tippecanoe   |
($pipeline[0].components   // [] | map(.["bom-ref"] // .name)) as $deps_of_pipeline     |

# Collect all known component bom-refs so we can filter out
# dependency entries that reference undeclared components
(
  [$image, $ref_pipeline, $ref_tippecanoe] +
  ($pipeline[0].components   // [] | map(.["bom-ref"] // .name)) +
  ($tippecanoe[0].components // [] | map(.["bom-ref"] // .name))
) as $known_refs |

# Carry forward inner deps from each fragment:
# - drop the fragment's own root entry (now represented at container level)
# - drop any entry whose ref is not a known component (stray entries)
# - deduplicate by ref (keep first occurrence)
(
  ($tippecanoe[0].dependencies // [] | map(select(.ref != $ref_tippecanoe))) +
  ($pipeline[0].dependencies   // [] | map(select(.ref != $ref_pipeline)))
  | map(select(.ref as $r | $known_refs | index($r) != null))
  | reduce .[] as $dep (
      [];
      if (map(.ref) | index($dep.ref)) == null then . + [$dep] else . end
    )
) as $inner_deps |

{
  bomFormat:    "CycloneDX",
  specVersion:  "1.7",
  version:      1,
  serialNumber: $serial,

  metadata: {
    timestamp: $timestamp,
    supplier: {
      name: "All The Places",
      url:  ["https://github.com/alltheplaces/"]
    },
    lifecycles: [{phase: "build"}],
    component: {
      type:      "container",
      "bom-ref": $image,
      name:      $image
      # version (the image digest) is patched in by release.yml, see above.
    },
    properties: (
      (($pipeline[0].metadata.properties // []) + ($tippecanoe[0].metadata.properties // []))
      | unique
    )
  },

  components: [
    ($meta_pipeline + {
      "bom-ref":  $ref_pipeline,
      type:       "application",
      components: ($pipeline[0].components // [])
    }),
    ($meta_tippecanoe + {
      "bom-ref":  $ref_tippecanoe,
      type:       "application",
      components: ($tippecanoe[0].components // [])
    })
  ],

  dependencies: (
    [
      {
        "ref":       $image,
        "dependsOn": [$ref_tippecanoe, $ref_pipeline]
      },
      {
        "ref":       $ref_tippecanoe,
        "dependsOn": $deps_of_tippecanoe
      },
      {
        "ref":       $ref_pipeline,
        "dependsOn": ($deps_of_pipeline + [$ref_tippecanoe])
      }
    ] + $inner_deps
  ),

  compositions: [
    {
      "aggregate":  "complete",
      "assemblies": [$ref_tippecanoe]
    },
    {
      "aggregate":  "complete",
      "assemblies": [$ref_pipeline]
    }
  ],

  formulation: (($pipeline[0].formulation // []) + ($tippecanoe[0].formulation // []) + [{
     workflows: [{
      "bom-ref": "assemble-container",
      uid: "assemble-container",
      name: "Assemble OCI container image",
      taskTypes: ["copy"],
      resourceReferences: [
        { "ref": $image },
        { "ref": $ref_pipeline },
        { "ref": $ref_tippecanoe }
      ],
      inputs: [
        { "resource": { "ref": $ref_pipeline } },
        { "resource": { "ref": $ref_tippecanoe } }
      ],
      outputs: [
        { "type": "artifact", "resource": { "ref": $image } }
      ]
    }]
  }])
}
