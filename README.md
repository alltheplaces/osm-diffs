# OSM Diffs

[![Coverage Status](https://coveralls.io/repos/github/brawer/osmdiffs/badge.svg?branch=main)](https://coveralls.io/github/brawer/osmdiffs?branch=main)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/brawer/osmdiffs/badge)](https://scorecard.dev/viewer/?uri=github.com/brawer/osmdiffs)
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
[#696](https://github.com/brawer/osmdiffs/issues/696).

Just want the data this pipeline produces? See
[`docs/outputs/`](docs/outputs/) for what’s in it and how to read it.

Want to build or run the pipeline itself? See
[`docs/CONTRIBUTING.md`](docs/CONTRIBUTING.md) to get started,
[`docs/TECHNICAL_DESIGN.md`](docs/TECHNICAL_DESIGN.md) for why this
exists and how it’s built, and [`docs/`](docs/) generally for testing,
cutting a release, and our security practices.
