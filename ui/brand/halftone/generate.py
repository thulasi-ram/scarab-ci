#!/usr/bin/env python3
"""Generate the halftone Scarab mark.

    python3 ui/brand/halftone/generate.py           write the SVGs
    python3 ui/brand/halftone/generate.py --check   fail if the SVGs on disk differ
    python3 ui/brand/halftone/generate.py --ico     build favicon.ico from the favicon PNGs

Standard library only. The PNGs are rendered from these SVGs by serve_png.py.

The mark is a beetle drawn in dots, inside a pointy-top hexagon with rounded
corners, outlined by 72 hexagonal dots. All sizes are in a 120-unit canvas.

How the beetle is built:
  * The body outline (head block, pronotum, wing cases) is a set of half-width
    tables measured on a dorsal photograph of Scarabaeus sacer, in photo pixels
    (x = offset from the body axis, y = row), made symmetric about the axis.
    No pixels from the photo are used.
  * The body is filled from a hex lattice. Dot size follows a light above the
    beetle (`tone`): the wing cases carry the largest dots, the pronotum has a
    highlight. The ink mark inverts the sizes, so lit areas get the smallest
    dots and shade the largest.
  * The rim, the horns and the six legs are explicit dot chains, built once on
    the right side and mirrored.
  * The beetle is scaled to BEETLE_SCALE and placed inside the ring.
"""
import math
import os
import struct
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
BRAND = os.path.dirname(HERE)                                     # ui/brand

# ---- palette (see ui/brand/README.md) ----
CREAM, INK, NIGHT = '#f4f1ea', '#111418', '#0b0c0e'

# ---- measured shape, in photo pixels ----
S = 0.0895                                   # photo px -> canvas units
def to_photo(X, Y): return ((X - 60) / S, 125 + (Y - 4) / S)

def interp(tab, y):
    if y <= tab[0][0]: return tab[0][1]
    for (y0, v0), (y1, v1) in zip(tab, tab[1:]):
        if y <= y1: return v0 + (v1 - v0) * (y - y0) / (y1 - y0)
    return tab[-1][1]

def ellipse_rear(front, y0, w0, y1=1341, n=14):
    pts = list(front)
    for i in range(1, n + 1):
        y = y0 + (y1 - y0) * i / n
        pts.append((round(y), round(w0 * math.sqrt(max(0, 1 - ((y - y0) / (y1 - y0)) ** 2)))))
    pts[-1] = (y1, 0)
    return pts

HEAD_HW = [(125, 140), (250, 215), (272, 215)]
PRON_HW = [(268, 125), (285, 165), (300, 205), (325, 242), (350, 278), (375, 305), (400, 328),
           (425, 345), (450, 356), (500, 362), (535, 356), (558, 335)]
ELY_HW = ellipse_rear([(560, 360), (650, 378), (780, 385), (900, 386)], 900, 386)   # oval rear

def clypeus(x, y):
    if y < 125 or y > 272 or abs(x) > interp(HEAD_HW, y): return False
    p = ((x + 140) / (280 / 6)) % 1
    top = 125 + 35 * abs(p - .5) * 2
    return y >= top if abs(x) <= 140 else y >= 125 + 35
def pronotum_bottom(x): return 636 - 78 * min(abs(x), 335) / 335
def pronotum(x, y): return 268 <= y <= pronotum_bottom(x) and abs(x) <= interp(PRON_HW, y)
def elytra(x, y):
    if y < pronotum_bottom(x) + 16 or y > 1341: return False
    return abs(x) <= interp(ELY_HW, y)
def body(x, y):                              # head block, pronotum, wing cases; legs and horns are separate
    return pronotum(x, y) or elytra(x, y) or (clypeus(x, y) and y >= 205)
def body_f(x, y):                            # the same with the V gap closed, for edge probes
    return pronotum(x, y) or (clypeus(x, y) and y >= 205) or (pronotum_bottom(x) - 2 <= y <= 1341 and abs(x) <= interp(ELY_HW, y))

def tone(x, y):                              # light from above, symmetric
    if pronotum(x, y):
        dd = math.hypot(x / 210, (y - 530) / 140)
        return .12 + .88 * math.exp(-dd * dd * 2.2)
    if elytra(x, y):
        lat = 1 - min(abs(x), 385) / 385
        t = .35 + .45 * lat ** .8 + .2 * (1 - (y - 650) / 690)
        return max(0, min(1, (t - .15) / .75))
    return .8

# ---- explicit dot chains: (x offset, y, radius), right side only ----
def along(pts, step_px):
    out, carry = [], 0.0
    for (ax, ay), (bx, by) in zip(pts, pts[1:]):
        L = math.hypot(bx - ax, by - ay); tx, ty = (bx - ax) / L, (by - ay) / L
        s = carry
        while s <= L:
            out.append((ax + tx * s, ay + ty * s, tx, ty)); s += step_px
        carry = s - L
    return out

def catmull(pts, per=14):
    P = [pts[0]] + pts + [pts[-1]]; out = []
    for i in range(1, len(P) - 2):
        p0, p1, p2, p3 = P[i - 1], P[i], P[i + 1], P[i + 2]
        for k in range(per):
            t = k / per
            out.append(tuple(.5 * ((2 * p1[j]) + (-p0[j] + p2[j]) * t + (2 * p0[j] - 5 * p1[j] + 4 * p2[j] - p3[j]) * t * t
                                   + (-p0[j] + 3 * p1[j] - 3 * p2[j] + p3[j]) * t ** 3) for j in (0, 1)))
    out.append(pts[-1]); return out

def inset_poly(pts, d, inside=(0, 950)):
    out = []
    for i, (x, y) in enumerate(pts):
        p, q = pts[max(0, i - 1)], pts[min(len(pts) - 1, i + 1)]
        tx, ty = q[0] - p[0], q[1] - p[1]; L = math.hypot(tx, ty) or 1; nx, ny = -ty / L, tx / L
        if (inside[0] - x) * nx + (inside[1] - y) * ny < 0: nx, ny = -nx, -ny
        out.append((x + nx * d, y + ny * d))
    return out

HORNS = []
def horn_chain(P0, P1, P2, P3, n=11):
    for i in range(n):
        t = i / (n - 1)
        x = (1 - t) ** 3 * P0[0] + 3 * (1 - t) ** 2 * t * P1[0] + 3 * (1 - t) * t ** 2 * P2[0] + t ** 3 * P3[0]
        y = (1 - t) ** 3 * P0[1] + 3 * (1 - t) ** 2 * t * P1[1] + 3 * (1 - t) * t ** 2 * P2[1] + t ** 3 * P3[1]
        HORNS.append((x, y, 1.9 - 1.25 * t))
horn_chain((150, 215), (250, 190), (300, 60), (225, -40))

# Legs: (femur path, tibia path). The second point of each is the knee.
LEGDEF = (
    ([(360, 420), (465, 440)], [(465, 440), (470, 560), (445, 650)]),     # fore
    ([(385, 690), (465, 705)], [(465, 705), (455, 860)]),                 # mid
    ([(385, 935), (480, 965)], [(480, 965), (452, 1130), (398, 1250), (338, 1322)]),   # hind
)
PARTS = []
def seg(pts, step, r0, r1, spines=False):
    ps = along(pts, step / S); n = len(ps)
    for i, (x, y, tx, ty) in enumerate(ps):
        r = r0 + (r1 - r0) * i / max(1, n - 1)
        PARTS.append((x, y, r))
        if spines and i % 3 == 1:
            nx, ny = -ty, tx
            if nx < 0: nx, ny = -nx, -ny
            PARTS.append((x + nx * (r + .5) / S, y + ny * (r + .5) / S, .42))
def foot(pt, d):                             # three tarsus dots, then two claws
    dx, dy = d; L = math.hypot(dx, dy); dx, dy = dx / L, dy / L
    for k, r in enumerate((.72, .58, .46), start=1):
        PARTS.append((pt[0] + dx * k * 3.0 / S, pt[1] + dy * k * 3.0 / S, r))
    e = (pt[0] + dx * 3.0 * 3 / S, pt[1] + dy * 3.0 * 3 / S)
    for ang in (-38, 38):
        a_ = math.radians(ang); cx_, cy_ = dx * math.cos(a_) - dy * math.sin(a_), dx * math.sin(a_) + dy * math.cos(a_)
        PARTS.append((e[0] + cx_ * 2.8 / S, e[1] + cy_ * 2.8 / S, .4))
for fem, tib in LEGDEF:
    seg(fem, 2.5, 1.75, 1.5)                                              # femur: thick
    PARTS.append((fem[1][0], fem[1][1], 1.95))                            # knee bulb
    seg(tib, 2.8, 1.15, .85, spines=True)                                 # tibia: thinner, spiny
    foot(tib[-1], (tib[-1][0] - tib[-2][0], tib[-1][1] - tib[-2][1]))

def units(parts, sx=1): return [(60 + sx * x * S, 4 + (y - 125) * S, r) for x, y, r in parts]
def both(parts):
    seen, res = set(), []
    for X, Y, r in units(parts, 1) + units(parts, -1):                    # centre-line dots appear once
        k = (round(X, 2), round(Y, 2))
        if k not in seen: seen.add(k); res.append((X, Y, r))
    return res

# ---- the rim: a smooth outline of dots just inside the body edge; the V between pronotum and wing cases stays open ----
RIM_HEAD = [(0, 219), (165, 219), (199, 244), (204, 262)]
RIM_PRON = [(150, 284), (191, 300), (228, 325), (264, 350), (291, 375), (314, 400), (331, 425), (342, 450), (348, 500), (342, 535), (322, 556)]
_ely = [(0, 1341)] + [(interp(ELY_HW, y), y) for y in range(1329, 579, -12)]
RIM_ELY = inset_poly(_ely, 14); RIM_ELY[0] = (0, 1327)                    # starts on the axis so the rear is symmetric

PROBES = [(math.cos(math.radians(a)) * 16, math.sin(math.radians(a)) * 16) for a in range(0, 360, 45)]
HEX_R = 64                                                                # trims the lattice; the whole body is inside

def rim_dots(inv):
    def rim_r(x, y):
        if inv: return max(.5, min(1.0, (2.0 - 1.2 * tone(x, y)) * .5))
        return max(.3, min(.9, (.3 + 1.7 * tone(x, y)) * .45))            # a fixed fraction of the interior size
    return [(x, y, rim_r(x, y)) for pl in (RIM_HEAD, RIM_PRON, RIM_ELY) for x, y, _, _ in along(catmull(pl), 3.1 / S)]

def beetle(inv, sp=3.9, rmin=.3, rmax=2.0):
    """The beetle's dots as (x, y, r) in unscaled canvas units. inv=True is the ink mark."""
    rim_all = rim_dots(inv)
    rim = both(rim_all)
    parts = both([p for p in PARTS if not body(p[0], p[1])]) + both(HORNS)
    dots = []
    for r in range(int(130 / (sp * .866))):
        Y = 3 + r * sp * .866
        for c in range(-40, 41):
            X = 60 + (c + (.5 if r % 2 else 0)) * sp
            dx, dy = abs(X - 60), abs(Y - 60)
            if X < 0 or X > 120 or Y > 119 or not (dx <= HEX_R * .866 and dy <= HEX_R - dx / 1.732): continue
            x, y = to_photo(X, Y)
            if not body(x, y): continue
            if any(not body_f(x + px_, y + py_) for px_, py_ in PROBES): continue      # the edge band belongs to the rim
            if inv:                                                                 # ink: lit areas get the smallest dots
                rr = 2.0 - 1.2 * tone(x, y)
                if abs(x) < 3 and y > 660: rr = min(rr, .3)                         # seam
                if abs(y - (pronotum_bottom(x) + 8)) < 30 and abs(x) < 400 and y > 400: rr = min(rr, .3)   # V gap
            else:
                rr = rmin + (rmax - rmin) * tone(x, y)
                rr *= .5 + .5 * min(1, min(math.hypot(X - a, Y - b) for a, b, _ in rim) / 4.5)   # fade toward the edge
                if abs(x) < 3 and y > 660: rr = min(rr, rmin)
                if abs(y - (pronotum_bottom(x) + 8)) < 30 and abs(x) < 400 and y > 400: rr = min(rr, rmin)
            for a, b, pr in rim:                                                    # sit beside the rim, never touch it
                dd = math.hypot(X - a, Y - b)
                if dd < rr + pr + .2: rr = min(rr, dd - pr - .2)
            if rr >= .25: dots.append((X, Y, round(rr, 2)))
    return [d for d in dots + rim + parts if d[2] > 0]

# ---- the ring: a rounded hexagon of 72 hexagonal dots ----
R, CX, CY = 55.5, 60.0, 60.0                 # centre-line circumradius and canvas centre
CORNER = 14                                  # corner radius
RING_DOTS, RING_DOT_R = 72, 1.7
BEETLE_SCALE, BEETLE_DY = 0.63, 0.5          # largest scale at which the horns and feet clear the ring

def _unit(a, b):
    d = math.hypot(b[0] - a[0], b[1] - a[1]); return ((b[0] - a[0]) / d, (b[1] - a[1]) / d)

def ring_loop(rc):
    """Dense clockwise polyline of the rounded hexagon, starting at the top corner's mid-arc."""
    V = [(CX + R * math.cos(math.radians(90 + 60 * k)), CY - R * math.sin(math.radians(90 + 60 * k))) for k in range(6)]
    cw = [0, 5, 4, 3, 2, 1]
    t = rc / math.tan(math.radians(60)); arcs = []
    for i, k in enumerate(cw):
        P, Vk, N = V[cw[i - 1]], V[k], V[cw[(i + 1) % 6]]
        up, un, uc = _unit(Vk, P), _unit(Vk, N), _unit(Vk, (CX, CY))
        tin = (Vk[0] + up[0] * t, Vk[1] + up[1] * t); tout = (Vk[0] + un[0] * t, Vk[1] + un[1] * t)
        c = (Vk[0] + uc[0] * rc / math.sin(math.radians(60)), Vk[1] + uc[1] * rc / math.sin(math.radians(60)))
        a0 = math.atan2(tin[1] - c[1], tin[0] - c[0]); a1 = math.atan2(tout[1] - c[1], tout[0] - c[0])
        while a1 < a0: a1 += 2 * math.pi
        arcs.append((c, a0, a1, tin, tout))
    out = []; mid = 0
    for i, (c, a0, a1, tin, tout) in enumerate(arcs):
        M = 80
        arc = [(c[0] + rc * math.cos(a0 + (a1 - a0) * j / M), c[1] + rc * math.sin(a0 + (a1 - a0) * j / M)) for j in range(M + 1)]
        nxt_in = arcs[(i + 1) % 6][3]
        line = [(tout[0] + (nxt_in[0] - tout[0]) * j / 200, tout[1] + (nxt_in[1] - tout[1]) * j / 200) for j in range(1, 200)]
        if i == 0: mid = 40
        out += arc[:-1] + [arc[-1]] + line
    return out[mid:] + out[:mid]

def resample(poly, n_half, total):
    cum = [0.0]
    for a, b in zip(poly, poly[1:] + poly[:1]): cum.append(cum[-1] + math.hypot(b[0] - a[0], b[1] - a[1]))
    L = cum[-1]; res = []
    for k in range(n_half + 1):
        s = L * k / total; j = max(1, min(len(cum) - 1, next(i for i in range(len(cum)) if cum[i] >= s)))
        f = 0 if cum[j] == cum[j - 1] else (s - cum[j - 1]) / (cum[j] - cum[j - 1])
        a = poly[(j - 1) % len(poly)]; b = poly[j % len(poly)]
        res.append((a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f))
    return res

def hexdot(cx, cy, r):                       # pointy-top regular hexagon: its sides run parallel to the ring's sides
    return 'M' + 'L'.join(f'{cx + r * math.cos(math.radians(90 + 60 * k)):.2f} {cy - r * math.sin(math.radians(90 + 60 * k)):.2f}' for k in range(6)) + 'Z'

def ring_path():
    half = resample(ring_loop(CORNER), RING_DOTS // 2, RING_DOTS)
    pts = [half[0]] + [p for p in half[1:-1]] + [half[-1]] + [(2 * CX - p[0], p[1]) for p in half[1:-1]]   # right half, mirrored
    assert len(pts) == RING_DOTS
    return ''.join(hexdot(x, y, RING_DOT_R) for x, y in pts)

# ---- SVG output ----
def body_markup(inv):
    """Beetle circles followed by the ring path, ready to sit inside <g fill=...>."""
    ds = beetle(inv); s, ty = BEETLE_SCALE, BEETLE_DY
    circles = ''.join(f'<circle cx="{CX + s * (x - CX):.2f}" cy="{CY + s * (y - 58) + ty:.2f}" r="{s * r:.2f}"/>'
                      for x, y, r in sorted(ds, key=lambda p: (round(p[1], 1), p[0])))
    return circles + f'<path d="{ring_path()}"/>'

VB = '0 0 120 120'
def svg(col, inner):
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{VB}" role="img" aria-label="Scarab"><title>Scarab</title><g fill="{col}">{inner}</g></svg>\n'

def outputs():
    cc, ci = body_markup(False), body_markup(True)
    tile = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{VB}" role="img" aria-label="Scarab"><title>Scarab</title>'
            f'<rect width="120" height="120" fill="{NIGHT}"/>'
            f'<g transform="translate(60 60) scale(.86) translate(-60 -60)" fill="{CREAM}">{cc}</g></svg>\n')
    favicon = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{VB}" role="img" aria-label="Scarab"><title>Scarab</title>'
               '<style>.d{display:none}@media (prefers-color-scheme:dark){.l{display:none}.d{display:inline}}</style>'
               f'<g class="l" fill="{INK}">{ci}</g><g class="d" fill="{CREAM}">{cc}</g></svg>\n')
    return {
        'logo/scarab-halftone.svg': svg(CREAM, cc),
        'logo/scarab-halftone-ink.svg': svg(INK, ci),
        'logo/scarab-halftone-tile.svg': tile,
        'favicon/halftone/favicon.svg': favicon,
    }

def build_ico():
    d = os.path.join(BRAND, 'favicon', 'halftone')
    sizes = [16, 32, 48, 64]; pngs = [open(os.path.join(d, f'favicon-{s}.png'), 'rb').read() for s in sizes]
    hdr = struct.pack('<HHH', 0, 1, len(sizes)); off = 6 + 16 * len(sizes); ent = b''; data = b''
    for s, p in zip(sizes, pngs):
        ent += struct.pack('<BBBBHHII', s, s, 0, 0, 1, 32, len(p), off + len(data)); data += p
    open(os.path.join(d, 'favicon.ico'), 'wb').write(hdr + ent + data)
    print('wrote favicon/halftone/favicon.ico from', ', '.join(f'favicon-{s}.png' for s in sizes))

def main(argv):
    if '--ico' in argv: return build_ico()
    outs = outputs(); bad = 0
    for rel, text in outs.items():
        path = os.path.join(BRAND, rel)
        if '--check' in argv:
            same = os.path.exists(path) and open(path).read() == text
            print(('ok      ' if same else 'DIFFERS ') + rel); bad += not same
        else:
            os.makedirs(os.path.dirname(path), exist_ok=True); open(path, 'w').write(text); print('wrote', rel)
    return 1 if bad else 0

if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
