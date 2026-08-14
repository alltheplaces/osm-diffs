# Test data

Small, illustrative log excerpts in exactly the shape `logs/<name>/`
holds after a real `cloud_test.py logs` -- both as documentation of
what these logs actually look like, and as fixtures for exercising
`analyze.py`.

Assembled from real values captured during the PR 665 (`OsmFeatureIndex`/
`conflate`) Hetzner experiment (see
[#667](https://github.com/alltheplaces/osm-diffs/issues/667)) -- real
message text, field names, and typical magnitudes -- but reassembled and
re-timestamped into one small, internally-consistent example rather than
a verbatim capture of any single run. In particular, `pipeline.log`
includes a real failure mode worth having an example of: `build_coverage`
hitting `EMFILE` (too many open files) on its first attempt, succeeding
on a retry after raising the file descriptor limit.

Try it:

```console
$ ./analyze.py timeline test_data/pipeline.log
$ ./analyze.py vmstat-stats test_data/vmstat.log --step build_coverage
$ ./analyze.py disk-stats test_data/disk.log --step build_coverage
```
