# Beetle sprite sheet

The brand beetle as pixel art: three poses (sleep / idle / rear), drawn in four
reds. Everything downstream reads the pixel GRID, never the pixels themselves.

`beetle-sheet.png` is a **de-noised transcription** of the sheet as supplied,
not the supplied file itself. What arrived was a restoration: the same artwork
upscaled and speckled, 1.0 MB of which was noise. Every cell was read at the
native pitch and painted back flat — same dimensions, same positions, same four
inks — which is 6.8 KB and, being on-grid, is also now legible as pixel art
rather than as a photograph of some. The swap was gated on a round-trip:
`trace()` over the transcription returns byte-identical poses to `trace()` over
the original. The original restoration is not in the repo.

`node trace-sprites.mjs` regenerates the two derived files; both are committed
so the art shows up in diffs:

| file | what |
|---|---|
| `beetle-poses.json` | the three poses as cell grids in the sheet's own four shades — a faithful transcription, no brand decisions baked in — plus the measured pitch, palette and anatomy |
| `beetle-poses.png` | the same poses in **brand** colours at 8×, for review |

## Facts measured off the sheet (don't re-derive them)

- **Binary alpha**: background `< 128`, ink `>= 128`.
- **Native pixel pitch 15.5 source px**, grid phase `(12.5, 6)`. Found by
  autocorrelating ink-transition edges, then confirmed by a cell-purity scan:
  97% of cells are unanimous at this pitch and phase.
- **Four inks**, from k-means over per-cell median RGB. Dark to light:
  outline `#850603`, shadow `#c40805`, body `#d61b13`, highlight `#fe8782`.
- The sprites are **~25 × 20 native px**. That is the whole design.

## Two rules the scenes inherit

- **Use it 1:1.** At 1.5× the horn's arch breaks into disconnected cells and
  the front of the beetle turns into a mammoth's head. The scenes place one
  source pixel per baked cell. Rotation is fine (see `scenes.mjs`) — scaling
  is not.
- **Colour by anatomy, not by shade.** The brand layers are semantic — shell
  emerald, head/pronotum/horn gold, limbs gray — so the mapping runs off the
  `parts` annotation (`seam`, `legRow`), not off which of the four reds a cell
  happens to be. `rear` is unannotated because no scene uses it; it renders
  flat in the review sheet.

Every beetle on the sheet faces **left**. Scenes mirror as needed.
