# Scarab brand assets

## Palette

| Token       | Hex       | Role                                                   |
|-------------|-----------|--------------------------------------------------------|
| Ink         | `#272a31` | Armour of the mark on light surfaces, one-colour mark  |
| Gold        | `#c9a94e` | Fill of the mark (wing case, horn band, abdomen)       |
| Cream       | `#f4f1ea` | Armour of the mark on dark surfaces, one-colour mark   |
| Navy        | `#1e2642` | Armour of the navy colourway                           |
| Night       | `#0b0c0e` | Background of the app-icon tile                        |
| Gold (deep) | `#9c7a2f` | Earlier emblem only                                    |
| Emerald     | `#1f9e74` | Earlier emblem only                                    |

Ink is a softened near-black, lighter than the UI's text ink, so the gold sits
beside it without a hard jump in contrast.

## The mark

A rhinoceros beetle seen from the side, walking left to right, horn raised. It
has two tones: the armour (shell rim, head and horn, legs) and the gold fill
(wing case, a band along the horn, abdomen). A slit along the top of the head
follows its curve, like the shine line between the shell rim and the wing case.

Use it in two tones from 48 px up: ink and gold on light surfaces, cream and
gold on dark ones. Use the one-colour mark (`mono`, `small`) for favicons, for
16 to 32 px, and where only one colour is available.

| File | What it is |
|------|-----------|
| `logo/scarab.svg` | ink and gold, transparent background, 480 x 328 |
| `logo/scarab-dark.svg` | cream and gold, for dark surfaces |
| `logo/scarab-navy.svg` | navy and gold |
| `logo/scarab-mono-{ink,cream}.svg` | one colour, 480 x 328 |
| `logo/scarab-square{,-dark}.svg` | the two-tone mark centred in a 512 square |
| `logo/scarab-small-{ink,cream}.svg` | the one-colour mark edge to edge in a 512 square, for 16 to 32 px |
| `logo/scarab-tile.svg` | cream and gold on an opaque `#0b0c0e` square |
| `logo/scarab-small-tile.svg` | the one-colour cream mark on the same tile |
| `icons/{light,dark,tile}/icon-{16..1024}.png` | PNGs of the square and tile SVGs; 16 and 32 px use the one-colour mark |
| `favicon/favicon.svg` | the one-colour mark, theme-aware: ink on a light tab, cream on a dark one |
| `favicon/favicon.ico`, `favicon-{16,32,48,64}.png` | the one-colour mark in ink, for browsers that ignore the SVG |
| `favicon/apple-touch-icon.png`, `icon-192.png`, `icon-512.png` | the tile, for home screens and app icons |

The web UI and the docs site serve copies of the favicon files from their
`public/` folders and of `scarab.svg` and `scarab-dark.svg` from
`src/assets/brand/`. Copy them again after regenerating.

## How it is made

`mark/source.png` is the artwork. `mark/trace.py` turns it into three masters,
`mark/armour.svg`, `mark/fill.svg` and `mark/mono.svg`, editing it on the way:

1. classify each pixel as background, armour or fill;
2. close the triangle on the head and the eye;
3. cut the head slit: an arc at a constant distance inside the head's top
   edge, widest near the shoulder and pointed at both ends;
4. replace the legs: same pose, tapering to a segmented foot with a small
   hooked claw (the drawn legs ended in boot shapes);
5. rebuild the abdomen as one convex shape under the new legs, and cut a gap
   of one width around every part drawn over another;
6. trace with potrace and write one absolute path per master.

`trace.py` needs numpy, opencv-python-headless, pillow and the `potrace`
binary. Run it only when the artwork or the edits change. Everything after it
uses the standard library or a browser:

```
just brand         # write the SVGs in logo/ and favicon/ from the masters
just brand check   # fail if those SVGs differ from the generator
just brand png     # re-render the 37 PNGs in a browser, then rebuild favicon.ico
```

`mark/serve_png.py` and `mark/raster.html` draw each SVG at every size in a
browser and save the PNGs; the server only writes `.png` files under
`icons/{light,dark,tile}/` and `favicon/`.

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
