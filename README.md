# kyyn-plugins

The first-party [kyyn](https://github.com/drshade/kyyn) tap: the
`sweep`, `kb`, and Microsoft Graph family plugins, served over the tap
harness.

A tap is a plugin repository a KB pins at an immutable commit in its
`sources.ron`; kyyn clones and builds it on first use. This repo is
what a fresh `kyyn init` pins — and the reference example for writing
your own tap: a `kyyn-tap.ron` manifest at the root, one binary calling
`kyyn_core::plugin::tap_main` with a plugin table.
