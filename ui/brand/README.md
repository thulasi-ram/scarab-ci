# Scarab brand assets

Scarab mark traced pixel-to-pixel from the source artwork (gold linework only,
no photographic background). Curves are shared across every variant; only the
fills / outline colors differ.

## Palette

| Token       | Hex       | Role                                    |
|-------------|-----------|-----------------------------------------|
| Gold        | `#c9a94e` | Body fill (light), outline (dark)       |
| Gold (deep) | `#9c7a2f` | Body fill on the dark variant           |
| Emerald     | `#1f9e74` | Wing fill (both variants)               |
| Ink         | `#111418` | Outline / border (light variant)        |
| Cream       | `#f4f1ea` | Dots of the halftone mark               |
| Night       | `#0b0c0e` | Background of the halftone tile         |

## Halftone mark

A scarab drawn in dots, inside a pointy-top hexagon outlined by 72 hexagonal
dots. The hexagon has rounded corners (radius 14 on the 120-unit canvas, where
the sides are 55.5 long) and the dots are spaced evenly along it. Each ring
dot is a small hexagon with its point up, so its sides run parallel to the
outline's vertical and slanted sides. Dot size in the beetle follows a light
above it: the wing cases carry the largest dots and the pronotum has a
highlight. The head carries a pair of curved horns, and the six legs have a
thick femur, a knee, a thinner spiny tibia and a small foot.

| File | What it is |
|------|-----------|
| `logo/scarab-halftone.svg` | cream dots on a transparent background, for dark surfaces |
| `logo/scarab-halftone-ink.svg` | ink dots on a transparent background, for light surfaces |
| `logo/scarab-halftone-tile.svg` | the cream mark on an opaque `#0b0c0e` square |
| `icons/halftone/icon-{16..1024}.png` | transparent PNGs of the cream mark |
| `icons/halftone-ink/icon-{16..1024}.png` | transparent PNGs of the ink mark |
| `icons/halftone-tile/icon-{16..1024}.png` | opaque PNGs of the tile; use these for app icons (apple-touch 180, PWA 192 and 512) |
| `favicon/halftone/favicon.svg` | the full mark with the ring, theme-aware: ink on light, cream on dark (60 KB) |
| `favicon/halftone/favicon.ico`, `favicon-{16,32,48,64}.png` | emerald mark on a transparent background, for browsers that ignore the SVG; emerald reads on light and dark tabs (about 3.4:1 on white, 4.7:1 on `#202124`) |
| `favicon/halftone/apple-touch-icon.png`, `icon-192.png`, `icon-512.png` | cream mark on a square `#0b0c0e` tile |

The ink mark is not the cream mark with the colour changed. The shading is
inverted, so the lit wing cases get the smallest dots and the shaded pronotum
gets the largest. A plain colour swap would draw the lit areas as the darkest.

The ring replaces an earlier field of tiny dots. The beetle is scaled to about
63% of its size in that version so its horns and hind feet sit inside the ring
with a gap of about 2.4 units.

Regenerating:

```
just brand         # write the four SVGs from halftone/generate.py
just brand check   # fail if the SVGs on disk differ from the generator
just brand png     # re-render the 37 PNGs in a browser, then rebuild favicon.ico
```

`halftone/generate.py` uses only the standard library. It holds the measured
body tables, the light model, the leg, horn and rim chains and the ring.
`halftone/serve_png.py` and `halftone/raster.html` draw each SVG at every size in
a browser and save the PNGs; the server only writes `.png` files under the four
icon folders. Re-rendering gave byte-identical files when we checked, on one
machine and browser. The SVGs are the source: edit the generator, run
`just brand`, then `just brand png`.

Status and limits:

- The web UI (header, login, favicon), the docs site (logo, home-page hero,
  favicon) and the repo's root `README.md` use this mark. The emblem files
  below are unchanged, and the ASCII beetle scenes in `ascii/` are still baked
  from the emblem.
- At 16 to 32 px the beetle blurs into a shaded shape that keeps its horns and
  legs, and the ring shows as a faint dotted hexagon. Contrast there is low,
  measured as the mean contrast of the mark against its background over the
  whole mark, ring included: about 1.2:1 on white and 1.7:1 on a dark tab at
  32 px. The earlier version without the ring measured 1.3:1 and 2:1; part of
  the drop is the empty area inside the ring, and part is the smaller beetle.
  A solid silhouette measures 2.4:1 and 8.9:1. A version with larger dots, a
  narrower gutter and thicker legs measured about 1.9:1 and 5:1 and is not in
  the repo.
- The body proportions were measured on a dorsal photograph of *Scarabaeus
  sacer* (Wikimedia Commons, `Scarabaeus.sacer.jpg`). No pixels from the photo
  are in the files.
- The figure is mirror-symmetric: every dot has a partner (checked on the SVG).
  The SVG has 629 circles and 72 hexagons.

## Primary emblem

The chosen mark: **emerald wings, gold body, transparent inside the circle**
(no disc fill), so it works on any background.

- **Light variant** — `scarab-emblem` — **ink outline**, gold body. For white /
  cream / navy / mid backgrounds.
- **Dark variant** — `scarab-emblem-dark` — **gold outline**, deep-gold body
  (keeps the beetle's internal lines visible against the fill). For black /
  near-black backgrounds, where an ink outline would vanish.

Each ships full-aspect (`viewBox 0 0 2196 1952`) and centered-square
(`128 -10 1920 1920`) for icons.

## Layout

```
logo/
  scarab-emblem.svg               PRIMARY (light) — ink outline, gold body, emerald wings
  scarab-emblem-square.svg        square crop for icons
  scarab-emblem-dark.svg          dark variant — gold outline, deep-gold body
  scarab-emblem-dark-square.svg
  scarab-gold.svg                 original single-color line trace (monochrome use)

icons/
  emblem/       icon-{16..1024}.png       light emblem, transparent
  emblem-dark/  icon-{16..1024}.png       dark emblem, transparent

favicon/
  favicon.svg          THEME-AWARE: ink outline on light, gold outline on dark
  favicon.ico          light emblem, multi-res 16/32/48/64
  favicon-16/32/48.png light emblem
  apple-touch-icon.png 180
  icon-192.png, icon-512.png   PWA
```

## Wiring (reference)

```html
<link rel="icon" href="/favicon.svg" type="image/svg+xml" />
<link rel="icon" href="/favicon.ico" sizes="any" />
<link rel="apple-touch-icon" href="/apple-touch-icon.png" />
```

The SVG favicon is theme-aware and should be preferred; the `.ico`/PNGs are the
raster fallback.

## Notes

- **Backgrounds:** the light emblem's ink outline disappears on black — use the
  dark variant there (or the theme-aware `favicon.svg`, which flips automatically).
- **Small sizes:** the emblem is detailed line-art. At a true 16px tab it softens;
  32px+ and the SVG favicon (which most browsers and hidpi screens use) are crisp.
- **Regenerating rasters:** rendered at 1024px via Quick Look on white *and*
  black, then alpha reconstructed from the two mattes (exact anti-aliased
  transparency for multi-color art). See the generator notes in the repo history.
```
