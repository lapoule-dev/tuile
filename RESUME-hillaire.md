# Resume — Hillaire 2020 atmosphere (branch feat/atmosphere-hillaire)

Worktree: /Users/laurentvaldes/Projects/tuile-atmosphere-hillaire
Rebased on origin/main c68f445 (PR #56 merged). Two clean commits:
ec9f60c feat(atmosphere) + fb6d9d4 feat(film). Old WIP kept locally as
branch backup/atmosphere-hillaire-wip (934d494).

## Done (2026-10-16 evening)
- Suite green, mutation-checked: night floor, continuity, sky near sun,
  zenith depth, new sky-brightness test, new no-light GPU test, new
  reflectance test.
- Sky "over-exposure": the model is physical (zenith 0.11-0.13 of a white
  Lambert ground, horizon 2.3x zenith). Real fault: air scaled against
  white texels while decoded imagery averages 0.012 for ~0.08-reflectance
  ground -> blue veil. Fix: Look::reflectance (6.5 decoded, 0.7 stored),
  Calibration::reflectance divides illuminance + floor.
- No-atmosphere frame 4500 byte-identical to 77beccb binary (cmp).
- Renders in tuile/videos (README entry, uncommitted): sun 55/20/5/-2.

## Remaining
- Push + PR (see report), if not done.
- Open: reflectance measured on one film; later derive per film
  (tuile-radiometry). No sky visible in frame 4500 — judge sky on a
  horizon-looking frame.
