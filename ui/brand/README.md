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

A scarab drawn as a field of dots inside a hexagon. Dot size follows a light
above the beetle: the wing cases carry the largest dots and the pronotum has a
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
| `favicon/halftone/favicon.svg` | the same mark without the hexagon dots, theme-aware: ink on light, cream on dark |
| `favicon/halftone/favicon.ico`, `favicon-{16,32,48,64}.png` | cream mark on a rounded `#0b0c0e` tile, visible on any tab colour |
| `favicon/halftone/apple-touch-icon.png`, `icon-192.png`, `icon-512.png` | cream mark on a square `#0b0c0e` tile |

The ink mark is not the cream mark with the colour changed. The shading is
inverted, so the lit wing cases get the smallest dots and the shaded pronotum
gets the largest. A plain colour swap would draw the lit areas as the darkest.

The favicon files drop the hexagon dots: they are invisible at tab size, and
without them the file is half the size and the beetle fills more of the icon.

Status and limits:

- It sits beside the emblem. The docs logo and the web UI still use the
  emblem; nothing is rewired to this mark yet.
- At 16 to 32 px the dots blur into a shaded beetle that keeps its horns and
  legs. Its contrast there is lower than a solid mark's: measured as the mean
  contrast of the mark against its background at 32 px, about 1.3:1 on white
  and 2:1 on a dark tab, against 2.4:1 and 8.9:1 for a solid silhouette. A
  version with larger dots, a narrower gutter and thicker legs measured about
  1.9:1 and 5:1, and is not in the repo.
- The body proportions were measured on a dorsal photograph of *Scarabaeus
  sacer* (Wikimedia Commons, `Scarabaeus.sacer.jpg`). No pixels from the photo
  are in the files.
- The figure is mirror-symmetric: every dot has a partner (checked in the
  generator). The SVG has 931 circles.
- The generator is a set of Python scripts that is not in this repo yet, so
  the SVG is the source for now.

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
