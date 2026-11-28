# The preload reservoir view

A view of what the graphics budget holds, and where, around and under the eye:
to see the ring as the decider produces it today, so as to judge a new score
against it. It decides nothing and changes no decision.

## What it shows

Opened from `Budget > Budget Window: Preload Reservoir`, `tuile://budget?window=reservoir`
(`reservoir_level=N|auto` for the level), or `V` in the budget window. It is a
third view of the budget window (board, tables, reservoir), in the same window
on a desktop, a page and a phone: the code is `tuile-ui::reservoir`, the same
everywhere.

- **Plan**: a nested mosaic of the terrain tiles of every level shown, each
  tile at its own size, coarse first so that finer tiles lie over them, 17
  tiles of the selected level wide around the eye, north up, the eye at the
  centre with its heading; the camera's footprint on the ground and the cone
  to its far corners in white. A tile drawn is solid green at any level
  (brighter at the selected level). At the selected level a held tile is
  filled by its score; at other levels a held tile is a faint fill and an
  outline, so that a held parent and its drawn children are both seen. A black
  cell means "not held", nothing else; a real hole is orange. `[` `]` change
  the selected level, `A` is auto (the finest level with at least three tiles
  held and not drawn near the eye, else the level of most drawn tiles), `B`
  and `U` hide the levels below and above, `W` limits the span to 2, 1 or all.
  The line under the plan counts the cells laid out by state; a test checks
  that, with every tile in the plan, they are the report's counts by level and
  state.
- **Section**: the tile under the eye at each level, with its state and score.
- **Colour**: drawn is green; in flight sky blue; queued grey; missing (a hole)
  vermilion; held and not drawn is on a fixed score ramp (indigo cold, pale
  yellow hot, `log2` of the score, 2^20 and above hottest). The ramp is
  monotone in luminance. A frame says why a held tile is held: white for the
  ring, orange for ancestors, a dark square for what is left over.
- **Header**: counts by state, the left-over count against the preloads held on
  purpose, resources by class, drawn tiles out of the present view,
  traversals truncated, held tiles by reason.
- **Hover** gives the tile, state, score, `A`, `D`, `P`, `R`, bytes, distance
  band and reason. **Click** picks a tile: it is outlined in the main view in
  its state colour (lines in the overlay, nothing in the ground's mesh), with
  an arrow and a distance at the edge when it is out of the frame. A second
  click, or Backspace, lets it go.

## Where the data comes from

Only what the budget publishes: `HeldReport` gains `ring`, a list of
`RingTile` (resource, class, level, band, terms, state, and the urgency tier
the tile was wanted at — the want's tier, else its load rank). It is built by
`Budget::reservoir_report`, a read-only walk of the arena that the runtime asks
for only while a window shows the reservoir (`metrics::want_reservoir`); with
the window closed `held_report` runs as before and nothing is added. `decide`
is untouched: decisions, digests, goldens and `budget_sim` do not move.

Cost with the window open: in a release build, 144 us per report for 699 tiles
(0.2 us a tile) against 37 us for `held_report`; a real session lists some
4 500 resources, about 1 ms every half second. Closed: none.

## Tiles drawn out of view

The header counts the terrain tiles marked drawn that no frustum of the present
view sees (`reservoir::drawn_outside`: a sphere at the tile's ground centre,
its half diagonal and 60 m of relief, against the same `Frustum` the traversal
culls with — which is already widened by `CULL_MARGIN`). On the test view
(44.8947 N, 0.5212 W, 801 m, heading 20, pitch 50) 1196 of 1480 drawn tiles are
out, all of them at levels 20 and 21 and most of 18 and 19. The origin of the
marking is investigated apart (`fix/no-selection-outside-view`); this view only
counts.

## What the tests do not cover

- The plan assumes the geographic 2 x 1 tiling scheme (`GeographicTilingScheme`);
  another scheme would draw the wrong cells.
- The outline is laid at height 0 and the out-of-view test uses a relief margin:
  a tile on a steep slope may be outlined a little off.
- 3D Tiles resources are counted, not drawn (their ids are arena indices).
- Anticipated views (#282) are not tested against: only the present eyes and the
  host's views.
- The ground is not tinted for a picked drawn tile: only the outline.
- Colours and layout are checked by a unit test of the mapping and by looking at
  the window, not by a pixel test.
