#!/usr/bin/env python3
"""Trace the side-view beetle artwork (source.png) into the mark's three masters.

    python3 ui/brand/mark/trace.py

Needs numpy, opencv-python-headless and pillow, plus the `potrace` binary.
`generate.py` (standard library only) builds every logo and favicon SVG from
the masters this writes, so run this only when the artwork or the edits below
change:

  armour.svg  the dark parts: shell rim, head and horn, legs
  fill.svg    the gold parts: wing case, horn band, abdomen
  mono.svg    the two merged, for one-colour use

Edits made to the artwork, in order:
  * classify every pixel as background, armour (navy) or fill (gold);
  * close the triangle on the head and the eye;
  * cut a slit into the head instead: an arc at a constant distance inside the
    head's top edge, widest near the shoulder and pointed at both ends, like
    the shine line on the wing case;
  * replace the legs: same pose, tapering to a segmented foot with a small
    hooked claw (the drawn legs ended in boot shapes);
  * rebuild the abdomen as one convex shape so the new legs sit on it, with a
    GAP-wide moat around every part drawn over another.

Each master is one absolute path in a 480 x 328 box, the beetle centred and
464 units wide.
"""
import math, os, re, subprocess, tempfile
import numpy as np, cv2
from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
F = 6                    # supersampling
GAP = 3.2                # moat between overlapping parts, source pixels
SLIT = dict(x0=283, x1=354, inset=8.5, width=5.28)
# Legs: (chain of (x, y, width), ankle, toe, direction). Source pixels.
LEGS = [([(192, 256, 19), (160, 292, 15), (140, 312, 11)], (140, 312), (100, 323), -1),
        ([(277, 234, 19), (268, 274, 15), (246, 310, 11)], (246, 310), (208, 323), -1),
        ([(321, 227, 19), (350, 262, 15), (382, 306, 11)], (382, 306), (420, 323), 1)]
BOX_W, BOX_H, MARK_W = 480, 328, 464

def keep_large(m, area):
    n, lab, st, cen = cv2.connectedComponentsWithStats(m.astype(np.uint8), 8)
    return [(lab == i, cen[i] / F) for i in range(1, n) if st[i, 4] >= area]

def disk(r):
    r = max(1, int(round(r))); return cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (2 * r + 1, 2 * r + 1))

def P(x, y): return (int(round(x * F)), int(round(y * F)))

def classify():
    im = np.array(Image.open(os.path.join(HERE, 'source.png')).convert('RGB')).astype(np.float32)
    up = cv2.GaussianBlur(cv2.resize(im, None, fx=F, fy=F, interpolation=cv2.INTER_CUBIC), (0, 0), 1.5)
    ref = [(240, 240, 240), (30, 38, 66), (208, 162, 82)]   # background, armour, fill
    return np.stack([((up - np.array(c, np.float32)) ** 2).sum(-1) for c in ref], -1).argmin(-1)

def holes(m):
    inv = (~m).astype(np.uint8)
    cv2.floodFill(inv, np.zeros((m.shape[0] + 2, m.shape[1] + 2), np.uint8), (0, 0), 0)
    return inv > 0

def slit_polygon(armour):
    xs = np.arange(274, 372, 1.0); ys = []
    for x in xs:
        col = np.nonzero(armour[int(60 * F):int(160 * F), int(x * F)])[0]
        ys.append(col[0] / F + 60)
    c = np.polyfit(xs, ys, 3); f = np.poly1d(c); df = f.deriv()
    outer, inner = [], []
    for s in np.linspace(0, 1, 41):
        x = SLIT['x0'] + (SLIT['x1'] - SLIT['x0']) * s; y = f(x); d = df(x); L = math.hypot(1, d); nx, ny = -d / L, 1 / L
        w = SLIT['width'] * (math.sin(math.pi * s) ** 0.7) * (1 - 0.35 * s)
        outer.append((x + nx * SLIT['inset'], y + ny * SLIT['inset']))
        inner.append((x + nx * (SLIT['inset'] + w), y + ny * (SLIT['inset'] + w)))
    return outer + inner[::-1]

def legs_mask(shape):
    m = np.zeros(shape, np.uint8); cuts = np.zeros(shape, np.uint8)
    def quad(a, b):
        (x0, y0, w0), (x1, y1, w1) = a, b
        dx, dy = x1 - x0, y1 - y0; L = math.hypot(dx, dy); nx, ny = -dy / L, dx / L
        cv2.fillPoly(m, [np.array([P(x0 + nx * w0 / 2, y0 + ny * w0 / 2), P(x1 + nx * w1 / 2, y1 + ny * w1 / 2),
                                   P(x1 - nx * w1 / 2, y1 - ny * w1 / 2), P(x0 - nx * w0 / 2, y0 - ny * w0 / 2)])], 1)
    for chain, ankle, toe, d in LEGS:
        for a, b in zip(chain, chain[1:]): quad(a, b)
        for x, y, w in chain: cv2.circle(m, P(x, y), int(w / 2 * F), 1, -1)
        quad((*ankle, 8.5), (*toe, 4.6))                        # foot
        dx, dy = toe[0] - ankle[0], toe[1] - ankle[1]; L = math.hypot(dx, dy); nx, ny = -dy / L, dx / L
        for t in (1 / 3, 2 / 3):                                 # foot segments
            cx, cy = ankle[0] + dx * t, ankle[1] + dy * t; w = 8.5 + (4.6 - 8.5) * t + 2
            cv2.line(cuts, P(cx - nx * w / 2, cy - ny * w / 2), P(cx + nx * w / 2, cy + ny * w / 2), 1, int(1.6 * F))
        x, y = toe                                               # hooked claw
        cv2.fillPoly(m, [np.array([P(x, y - 3), P(x + d * 8, y - 1.6), P(x + d * 12, y + 3.4), P(x + d * 7, y + 1.8), P(x, y + 3)])], 1)
    return (m > 0) & ~(cuts > 0)

def trace(mask):
    with tempfile.TemporaryDirectory() as t:
        pbm, out = os.path.join(t, 'm.pbm'), os.path.join(t, 'm.svg')
        Image.fromarray(np.where(mask, 0, 255).astype(np.uint8)).save(pbm)
        subprocess.run(['potrace', '-s', '--flat', '-a', '1.0', '-O', '0.8', '-t', '30', '-u', '4', pbm, '-o', out], check=True)
        raw = open(out).read()
    vb_h = float(re.search(r'viewBox="0 0 [\d.]+ ([\d.]+)"', raw).group(1))
    sc = float(re.search(r'scale\(([\d.]+),', raw).group(1))
    toks = re.findall(r'[MmLlCcZz]|-?\d+\.?\d*', ' '.join(re.findall(r'<path d="([^"]+)"', raw, re.S)))
    segs, i, cx, cy, sx, sy, cmd = [], 0, 0, 0, 0, 0, None
    while i < len(toks):
        if toks[i] in 'MmLlCcZz': cmd = toks[i]; i += 1
        if cmd in 'Zz': segs.append(['Z']); cx, cy = sx, sy; continue
        n = 6 if cmd in 'Cc' else 2
        v = [float(x) for x in toks[i:i + n]]; i += n
        if cmd.islower(): v = [v[k] + (cx if k % 2 == 0 else cy) for k in range(n)]
        cx, cy = v[-2], v[-1]
        if cmd in 'Mm': sx, sy = cx, cy; segs.append(['M'] + v); cmd = 'L' if cmd == 'M' else 'l'
        else: segs.append([cmd.upper()] + v)
    return [[s[0]] + [s[k] * sc / F if k % 2 == 1 else (vb_h - s[k] * sc) / F for k in range(1, len(s))] for s in segs]

def write(name, segs, fit):
    s, tx, ty = fit
    fmt = lambda v: f'{v:.2f}'.rstrip('0').rstrip('.')
    parts = []
    for seg in segs:
        if seg[0] == 'Z': parts.append('Z'); continue
        parts.append(seg[0] + ' '.join(fmt(seg[k] * s + (tx if k % 2 == 1 else ty)) for k in range(1, len(seg))))
    d = ' '.join(parts).replace(' -', '-')
    svg = f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {BOX_W} {BOX_H}"><path fill-rule="evenodd" d="{d}"/></svg>\n'
    open(os.path.join(HERE, name), 'w').write(svg)
    print(f'wrote mark/{name} ({len(svg)} bytes)')

def main():
    lab = classify()
    armour_all = lab == 1
    fill_all = cv2.morphologyEx((lab == 2).astype(np.uint8), cv2.MORPH_OPEN, disk(5)) > 0   # drop anti-alias fringe
    body = np.zeros_like(armour_all)
    for m, c in keep_large(armour_all, 3000):
        if c[1] <= 255: body |= m                         # the drawn legs (lower components) are dropped
    gold, belly = np.zeros_like(body), np.zeros_like(body)
    for m, c in keep_large(fill_all, 3000):
        (belly if c[1] > 230 and c[0] < 330 else gold).__ior__(m)
    hull = cv2.convexHull(np.column_stack(np.nonzero(belly))[:, ::-1].astype(np.int32))
    abdomen = np.zeros(body.shape, np.uint8); cv2.fillPoly(abdomen, [hull], 1); abdomen = abdomen > 0
    hl = holes(body)
    eye = np.zeros_like(body)
    for m, c in keep_large(hl, 500):
        if abs(c[0] - 315) < 25 and abs(c[1] - 135) < 25: body |= m      # close the triangle on the head
        if abs(c[0] - 382) < 15 and abs(c[1] - 204) < 15: body |= m; eye |= m
    gold &= ~cv2.dilate(eye.astype(np.uint8), disk(2 * F)).astype(bool)    # no eye
    slit = np.zeros(body.shape, np.uint8)
    cv2.fillPoly(slit, [np.array([P(x, y) for x, y in slit_polygon(body)])], 1)
    body &= ~(slit > 0)
    legs = legs_mask(body.shape)
    moat = lambda m: cv2.dilate(m.astype(np.uint8), disk(GAP * F)) > 0
    armour = (body & ~moat(legs)) | legs
    fill = (gold & ~moat(legs)) | (abdomen & ~moat(body | gold | legs))
    mono = cv2.morphologyEx((armour | fill).astype(np.uint8), cv2.MORPH_CLOSE, disk(4)) > 0
    segs = {n: trace(m) for n, m in (('armour', armour), ('fill', fill), ('mono', mono))}
    xs = [s[k] for s in segs['mono'] for k in range(1, len(s), 2)]; ys = [s[k] for s in segs['mono'] for k in range(2, len(s), 2)]
    sc = MARK_W / (max(xs) - min(xs))
    fit = (sc, BOX_W / 2 - (min(xs) + max(xs)) / 2 * sc, BOX_H / 2 - (min(ys) + max(ys)) / 2 * sc)
    for n in segs: write(f'{n}.svg', segs[n], fit)

if __name__ == '__main__':
    main()
