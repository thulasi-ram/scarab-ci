# Scarab brand assets

## Palette

| Token       | Hex       | Role                                    |
|-------------|-----------|-----------------------------------------|
| Ink         | `#111418` | The mark on light surfaces              |
| Cream       | `#f4f1ea` | The mark on dark surfaces               |
| Emerald     | `#1f9e74` | Fallback favicons, the emerald tile     |
| Gold        | `#c9a94e` | Gold colourway                          |
| Night       | `#0b0c0e` | Background of the app-icon tile         |
| Gold (deep) | `#9c7a2f` | Earlier emblem only                     |

## The mark

A horned beetle seen from above, in one colour. The plates of the head, the
pronotum and the wing cases, and the joints of the six legs, are separated by
cuts of one width, so the mark keeps its detail in a single colour on any
background. The two halves are exact mirror images.

There are two shapes:

- **The mark** (`plates`): the beetle with its cuts. Use it from 48 px up.
- **The small-size mark** (`small`, `bold` in `mark/`): the silhouette made
  thicker, with no cuts. Use it at 16 to 32 px, where the cuts and the thin legs
  of the full mark blur away. The favicons use it.

| File | What it is |
|------|-----------|
| `logo/scarab-{ink,cream,emerald,gold}.svg` | the mark on a transparent background, in a 320 x 480 box |
| `logo/scarab-square-{ink,cream,emerald,gold}.svg` | the mark centred in a 512 square; the web UI, docs site and root README use the ink and cream ones |
| `logo/scarab-small-{ink,cream,emerald,gold}.svg` | the small-size mark in a 512 square |
| `logo/scarab-tile.svg` | cream mark on an opaque `#0b0c0e` square |
| `logo/scarab-tile-emerald.svg` | ink mark on an opaque emerald square |
| `logo/scarab-small-tile{,-emerald}.svg` | the same two tiles with the small-size mark |
| `icons/{ink,cream,tile,tile-emerald}/icon-{16..1024}.png` | PNGs of the square and tile SVGs; 16 and 32 px use the small-size mark |
| `favicon/favicon.svg` | the small-size mark, theme-aware: ink on a light tab, cream on a dark one |
| `favicon/favicon.ico`, `favicon-{16,32,48,64}.png` | the small-size mark in emerald, for browsers that ignore the SVG; emerald reads on light and dark tabs |
| `favicon/apple-touch-icon.png`, `icon-192.png`, `icon-512.png` | the cream tile, for home screens and app icons |

The web UI and the docs site serve copies of the favicon files from their
`public/` folders and of the square ink and cream SVGs from
`src/assets/brand/`. Copy them again after regenerating.

## How it is made

`mark/source.png` is the artwork. `mark/trace.py` turns it into the two
masters, `mark/plates.svg` and `mark/bold.svg`:

1. average the artwork with its mirror image, so the halves match exactly;
2. threshold the beetle's silhouette; the artwork's grey shading and highlight
   edges are dropped;
3. draw the plate and joint cuts, listed by hand in `CUTS` on the left half and
   mirrored;
4. trace with potrace and write one absolute path per master.

`trace.py` needs numpy, opencv-python-headless, pillow and the `potrace`
binary. Run it only when the artwork or the cuts change. Everything after it
uses the standard library or a browser:

```
just brand         # write the SVGs in logo/ and favicon/ from the masters
just brand check   # fail if those SVGs differ from the generator
just brand png     # re-render the 47 PNGs in a browser, then rebuild favicon.ico
```

`mark/serve_png.py` and `mark/raster.html` draw each SVG at every size in a
browser and save the PNGs; the server only writes `.png` files under
`icons/{ink,cream,tile,tile-emerald}/` and `favicon/`.

## Wiring

```html
<link rel="icon" href="/favicon.ico" sizes="32x32" />
<link rel="icon" href="/favicon.svg" type="image/svg+xml" />
<link rel="apple-touch-icon" href="/apple-touch-icon.png" />
```

The `.ico` goes first with a fixed size: with `sizes="any"`, Chrome picks it
over the SVG.

## Earlier emblem

The first mark: **emerald wings, gold body, transparent inside the circle**. It
is no longer used in the apps; the ASCII beetle scenes in `ascii/` are still
baked from `logo/scarab-emblem.svg`.

- **Light variant** — `scarab-emblem` — **ink outline**, gold body. For white /
  cream / navy / mid backgrounds.
- **Dark variant** — `scarab-emblem-dark` — **gold outline**, deep-gold body
  (keeps the beetle's internal lines visible against the fill). For black /
  near-black backgrounds, where an ink outline would vanish.

Each ships full-aspect (`viewBox 0 0 2196 1952`) and centered-square
(`128 -10 1920 1920`) for icons.

Files: `logo/scarab-emblem{,-square,-dark,-dark-square}.svg`,
`logo/scarab-emblem-gold.svg` (the original single-colour line trace) and
`icons/emblem{,-dark}/icon-{16..1024}.png`.
