# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Python script to generate the Rust code in src/pipeline/osm/generated.rs.
#
# To run it, execute "uv run scripts/generate_id_tagging_schema.py"
# from the project root.
#
# TODO: At the moment, this script extracts some data from the sources of the iD editor.
# An example is OpenStreetMap lifecycle prefixes, which is currently (July 2026) not yet
# available in the id-tagging-schema project. We expect this to be a temporary hack.
#
# https://github.com/openstreetmap/id-tagging-schema/issues/2623

from datetime import datetime, timezone
from io import StringIO
import json
import os
from urllib.request import urlopen, Request
import re
import uuid
import subprocess
from typing import Any, Callable

ID_REPO = "openstreetmap/iD"
SCHEMA_REPO = "openstreetmap/id-tagging-schema"


def main():
    id_release = query_latest_release(ID_REPO)  # "v2.41.2"
    id_license = json.loads(fetch_source(ID_REPO, id_release, "package.json"))[
        "license"
    ]  # "ISC"
    src_tags = fetch_source(ID_REPO, id_release, "modules/osm/tags.ts")
    lifecycle_prefixes = extract_lifecycle_prefixes(src_tags)
    area_keys_exceptions = extract_area_keys_exceptions(src_tags)

    schema_release = query_latest_release(SCHEMA_REPO)
    schema_package_url = f"pkg:github/{SCHEMA_REPO}@{schema_release}".lower()
    schema_license = json.loads(
        fetch_source(SCHEMA_REPO, schema_release, "package.json")
    )["license"]
    schema_presets = json.loads(
        fetch_source(SCHEMA_REPO, schema_release, "dist/presets.json")
    )
    area_keys = extract_area_keys(schema_presets, lifecycle_prefixes)

    generated_rust = generate_rust(
        lifecycle_prefixes,
        area_keys,
        area_keys_exceptions,
        schema_package_url,
        schema_license,
    )
    update("src/pipeline/osm/id_tagging_schema.rs", generated_rust)


def query_latest_release(repo: str) -> str:
    req = Request(
        f"https://api.github.com/repos/{repo}/releases/latest",
        headers={
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2026-03-10",
        },
    )
    with urlopen(req) as response:
        data = json.loads(response.read())
    return data["tag_name"]


def fetch_source(repo: str, tag: str, path: str) -> str:
    req = Request(f"https://raw.githubusercontent.com/{repo}/refs/tags/{tag}/{path}")
    with urlopen(req) as response:
        return response.read().decode("utf-8")


def remove_comments(src: str) -> str:
    lines = [line.split("//", 1)[0].strip() for line in src.splitlines()]
    return "\n".join([line for line in lines if line])


def parse_typescript_dict(src: str) -> dict[str, str | bool]:
    def escape(x):
        x = x.group(0)
        if x.startswith('"') or x in {"true", "false"}:
            return x
        else:
            return '"' + x + '"'

    src = re.sub(r"[a-zA-Z]\w*", escape, remove_comments(src))
    return json.loads(src.strip())


def extract_lifecycle_prefixes(src: str) -> set[str]:
    src = src.split("export const osmLifecyclePrefixes = {")[1].split("};")[0]
    return set(re.findall(r"(\w+):\s*true", remove_comments(src)))


def extract_area_keys_exceptions(src: str) -> dict[str, dict[str, bool]]:
    src = src.split("export const osmAreaKeysExceptions: TagDictionary<boolean> =")[1]
    src = src.split("};")[0] + "}"
    return parse_typescript_dict(src)


def extract_area_keys(
    presets: dict[str, Any], lifecycle_prefixes
) -> dict[str, set[str]]:
    # https://github.com/ideditor/id-area-keys/blob/d84c65b20bb5169066e2f462695176ff0056aef3/build.js#L20C7-L20C77
    ignore = {"area", "barrier", "highway", "footway", "railway", "type"}
    area_keys = {}
    for p in presets.values():
        if "area" in p["geometry"]:
            tag_key, _tag_value = next(iter(p["tags"].items()))
            tag_key = remove_lifecycle_prefix(tag_key, lifecycle_prefixes)
            if tag_key not in ignore:
                area_keys.setdefault(tag_key, set())
    for p in presets.values():
        if "line" in p["geometry"] and len(p["tags"]) > 0:
            tag_key, tag_value = next(iter(p["tags"].items()))
            tag_key = remove_lifecycle_prefix(tag_key, lifecycle_prefixes)
            if tag_key in area_keys and tag_value != "*":
                area_keys[tag_key].add(tag_value)
    return area_keys


def remove_lifecycle_prefix(tag: str, prefixes: set[str]) -> str:
    s = tag.split(":", 1)
    if len(s) == 2 and s[0] in prefixes:
        return s[1]
    else:
        return tag


def join_quoted_strings(s: [str]) -> str:
    return " | ".join('"%s"' % x for x in sorted(s))


def generate_rust_match_clause(
    area_keys: dict[str, set[str]],
    _area_keys_exceptions: dict[str, dict[str, bool]],
) -> str:
    # If these assertions ever fail, we’ll need to adjust our code generation.
    prefix_keys = {k for k in area_keys.keys() if k.endswith(":*")}
    assert prefix_keys == {"addr:*"}
    for pk in prefix_keys:
        assert len(area_keys[pk]) == 0  # expect no exceptions

    out = StringIO()
    unconditional_keys = set()
    clauses = []
    for key, exceptions in sorted(area_keys.items()):
        if key in prefix_keys:
            continue
        if len(exceptions) == 0:
            unconditional_keys.add(key)
        elif len(exceptions) == 1:
            # Avoid clippy warnings.
            clauses.append(
                '"%s" => { if value != "%s" { has_area_tag = true } }'
                % (key, list(exceptions)[0])
            )
        else:
            clauses.append(
                '"%s" if !matches!(value, %s) => { has_area_tag = true }'
                % (key, join_quoted_strings(exceptions))
            )
    clauses.insert(
        0, "%s => {has_area_tag = true}" % join_quoted_strings(unconditional_keys)
    )
    clauses.append("  _ => {}")

    out.write("match remove_lifecycle_prefix(key) {")
    out.write(", ".join(clauses))
    out.write("}")
    return out.getvalue()


def generate_rust(
    lifecycle_prefixes: set[str],
    area_keys: dict[str, set[str]],
    area_keys_exceptions: dict[str, dict[str, bool]],
    package_url: str,
    package_license: str,
) -> str:
    out = StringIO()
    match_clause = generate_rust_match_clause(area_keys, area_keys_exceptions)
    out.write(
        """
        // DO NOT EDIT - THIS FILE IS GENERATED
        //
        // Generated by a Python script in scripts/generate_id_tagging_schema.py,
        // from data at %s
        // which is released under the %s license.

        pub fn is_area<'a>(closed: bool, tags: impl Iterator<Item = (&'a str, &'a str)>) -> bool {
            if !closed {
                return false;
            }

            let mut has_area_tag = false;
            for (key, value) in tags {
                if key == "area" {
                    match value {
                        "yes" => return true,
                        "no" => return false,
                        _ => {}
                    }
                }

                // For any tags other than `area`, we don’t return immediately.
                // At this point, we don’t know if the iteration will later yield
                // `area=no`, which should win over any tag heuristics.
                //
                // Alternatively, we could do _two_ separate passes over the tags,
                // like the iD editor does in its JavaScript sources. However,
                // this has a (small) performance cost, which we can avoid by deciding
                // on area-ness in a single pass over the tags.

                // Surprisingly, it looks like not all keys in the `areaKeys` dictionary
                // of the iD editor are strictly _keys_ in the OSM sense. One single
                // entry is actually a _pattern_, for `addr:*`. From the  iD source code,
                // it’s not obvious if iD actually checks for prefixes; this might get
                // silently ignored (because looking up `addr:street` in a JavaScript
                // dictionary containing something for `addr:*` will not find anything).
                //
                // From a real-world perspective, it seems reasonable to interpret
                // OSM features with addresses as areas, not lines. Therefore, we test
                // for `addr:` prefixes, thus deviating from iD’s behavior, assuming
                // this is actually a bug in the iD editor.
                // https://github.com/openstreetmap/id-tagging-schema/issues/2623#issuecomment-5202324005
                if key.starts_with("addr:") {  // `addr:*` in exception table
                    has_area_tag = true;
                    continue;
                }

                %s
            }

            has_area_tag
        }

        fn remove_lifecycle_prefix(key: &str) -> &str {
            if let Some((prefix, rest)) = key.split_once(':')
                && matches!(prefix, %s) {
                rest
            } else {
                key
            }
        }

        #[cfg(test)]
        mod tests {
            use super::*;

            #[test]
            fn test_is_area() {
                // If a way is not closed, it is never an area, no matter how it is tagged.
                assert_eq!(is_area(false, [("area", "yes")].into_iter()), false);
                assert_eq!(is_area(true, [("area", "yes")].into_iter()), true);
                assert_eq!(is_area(true, [("area", "no")].into_iter()), false);

                // By default, a closed way is not an area, unless the tags say otherwise.
                assert_eq!(is_area(true, [].into_iter()), false);
                assert_eq!(is_area(true, [("foo", "bar")].into_iter()), false);

                // Should handle allowlists and denylists.
                assert_eq!(is_area(true, [("aerialway", "pylon")].into_iter()), true);
                assert_eq!(is_area(true, [("aerialway", "pylon"), ("area", "no")].into_iter()), false);
                assert_eq!(is_area(true, [("aerialway", "pylon"), ("area", "yes")].into_iter()), true);
                assert_eq!(is_area(true, [("aerialway", "goods")].into_iter()), false);
                assert_eq!(is_area(true, [("aerialway", "goods"), ("area", "no")].into_iter()), false);
                assert_eq!(is_area(true, [("aerialway", "goods"), ("area", "yes")].into_iter()), true);

                // Should recognize lifecycle prefixes, similar to how iD does it in its codebase.
                assert_eq!(is_area(true, [("planned:aerialway", "pylon")].into_iter()), true);
                assert_eq!(is_area(true, [("planned:aerialway", "goods")].into_iter()), false);
                assert_eq!(is_area(true, [("razed:building", "yes")].into_iter()), true);
            }

            #[test]
            fn test_remove_lifecycle_prefix() {
                assert_eq!(remove_lifecycle_prefix("planned:highway"), "highway");
                assert_eq!(remove_lifecycle_prefix("razed:highway"), "highway");
                assert_eq!(remove_lifecycle_prefix(""), "");
                assert_eq!(remove_lifecycle_prefix("foo"), "foo");
                assert_eq!(remove_lifecycle_prefix("foo:"), "foo:");
                assert_eq!(remove_lifecycle_prefix("foo:bar"), "foo:bar");
                assert_eq!(remove_lifecycle_prefix("foo:bar:"), "foo:bar:");
                assert_eq!(remove_lifecycle_prefix("foo:bar:baz"), "foo:bar:baz");
            }
        }
        """
        % (
            package_url,
            package_license,
            match_clause,
            join_quoted_strings(lifecycle_prefixes),
        )
    )
    result = subprocess.run(
        ["rustfmt", "--edition", "2024"],
        input=out.getvalue(),
        capture_output=True,
        text=True,
    )
    return result.stdout


def update(path: str, content: str) -> bool:
    path = source_file_path(path)
    with open(path, "r") as fp:
        old_content = fp.read()
    if content == old_content:
        return False
    with open(path, "w") as fp:
        fp.write(content)
    return True


def source_file_path(path: str) -> str:
    script_dir = os.path.dirname(os.path.abspath(__file__))
    file_path = os.path.join(script_dir, "..", path)
    return os.path.abspath(file_path)


if __name__ == "__main__":
    main()
