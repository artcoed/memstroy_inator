# Particle library

Particles are sprites with a "bouncy / alive" preset baked in.
Dropping one onto the canvas spawns an `ImageOverlay` that already
has these animation modifiers wired up:

- **Spin** at 90°/sec — continuous rotation.
- **Pulse** at 1.5 Hz, ±0.15 scale — gentle breathing.
- **Wobble** at 1.0 Hz, ±12 px in X & Y — slight drift.

Edit / mute / remove modifiers from the inspector after dropping
to taste.

Same drop-in workflow as the **Images** tab: PNG / JPG / WebP /
GIF files in this directory are picked up by the **Particles** tab
on the next library refresh.
