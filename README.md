# OSM Diffs

[![Coverage Status](https://coveralls.io/repos/github/alltheplaces/osm-diffs/badge.svg?branch=main)](https://coveralls.io/github/alltheplaces/osm-diffs?branch=main)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/alltheplaces/osm-diffs/badge)](https://scorecard.dev/viewer/?uri=github.com/alltheplaces/osm-diffs)
[![Project Status: WIP – Initial development is in progress, but there has not yet been a stable, usable release suitable for the public.](https://www.repostatus.org/badges/latest/wip.svg)](https://www.repostatus.org/#wip)

Once completed, this will be a pipeline to compute weekly diffs
between [AllThePlaces](https://alltheplaces.xyz/) and
[OpenStreetMap](https://www.openstreetmap.org/about).
As its output, the pipeline will generate edit suggestions,
with the intention to help OpenStreetMap to be more complete
and up to date.

At the moment, this is still work in progress; the pipeline does not
yet run on a weekly basis. Once it does, the plan is to automatically
feed edit proposals to tools such as
[MapRoulette](https://maproulette.org/), where human users can
manually check each edit before applying it to OpenStreetMap — which
edits go where is still being worked out, see
[#696](https://github.com/alltheplaces/osm-diffs/issues/696).

New here? See [`docs/CONTRIBUTING.md`](docs/CONTRIBUTING.md) to get
started, and [`docs/`](docs/) generally for testing, cutting a
release, and our security practices. Looking for what the pipeline
actually produces instead? See [`docs/outputs/`](docs/outputs/).
