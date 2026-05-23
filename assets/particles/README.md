# Particle library (legacy)

This directory used to back a dedicated **Particles** tab in the
editor's library panel — sprites dropped here spawned
`ImageOverlay`s with a "bouncy / alive" preset (Spin / Pulse /
Wobble) already wired up. The dedicated tab has been retired in
favour of authoring particles through the regular **Images** tab and
adding the modifiers manually from the inspector.

The folder is still scanned on launch so existing scene files that
reference assets from here continue to load. New particle assets
should be dropped into `assets/images/` instead.
