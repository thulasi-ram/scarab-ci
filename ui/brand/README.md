# Scarab brand assets

Scarab mark traced pixel-to-pixel from the source artwork (gold linework only —
no photographic background). Curves are shared across every variant; only the
fills / outline colors differ.

## Palette

| Token       | Hex       | Role                                    |
|-------------|-----------|-----------------------------------------|
| Gold        | `#c9a94e` | Body fill (light), outline (dark)       |
| Gold (deep) | `#9c7a2f` | Body fill on the dark variant           |
| Emerald     | `#1f9e74` | Wing fill (both variants)               |
| Ink         | `#111418` | Outline / border (light variant)        |

## Primary emblem

The chosen mark: **emerald wings, gold body, transparent inside the circle**
(no disc fill), so it drops onto any surface.

- **Light variant** — `scarab-emblem` — **ink outline**, gold body. For white /
  cream / navy / mid backgrounds.
- **Dark variant** — `scarab-emblem-dark` — **gold outline**, deep-gold body
  (keeps the beetle's internal lines visible against the fill). For black / very
  dark backgrounds, where an ink outline would vanish.

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
  32px+ and the SVG favicon (which most modern/hidpi contexts use) are crisp.
- **Regenerating rasters:** rendered at 1024px via Quick Look on white *and*
  black, then alpha reconstructed from the two mattes (exact anti-aliased
  transparency for multi-color art). See the generator notes in the repo history.
```
