# kindred-plugins-core

The first-party [kindred](https://github.com/drshade/kindred) tap: the
`sweep`, `kb`, and Microsoft Graph family plugins, served over the tap
harness.

A tap is a plugin repository a KB pins at an immutable commit in its
`sources.ron`; kindred clones and builds it on first use. This repo is
what a fresh `kindred init` pins — and the reference example for writing
your own tap: a `kindred-tap.ron` manifest at the root, one binary calling
`kindred_core::plugin::tap_main` with a plugin table.
