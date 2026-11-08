#!/usr/bin/env python3
"""Generate the Scarab mark's logo and favicon SVGs from the three traced masters.

    python3 ui/brand/mark/generate.py           write the SVGs
    python3 ui/brand/mark/generate.py --check   fail if the SVGs on disk differ
    python3 ui/brand/mark/generate.py --ico     build favicon.ico from the favicon PNGs

Standard library only. The masters (`armour.svg`, `fill.svg`, `mono.svg`) are
written by trace.py from source.png. Each holds one absolute path in a 480 x 328
box with the beetle centred and 464 units wide. This script scales those paths
into each output; the PNGs are then rendered from the outputs by serve_png.py.
"""
import os
import re
import struct
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
BRAND = os.path.realpath(os.path.join(HERE, '..'))

INK, CREAM, GOLD, NAVY, NIGHT = '#272a31', '#f4f1ea', '#c9a94e', '#1e2642', '#0b0c0e'
MASTER_W, MASTER_CX, MASTER_CY = 464, 240, 164


def master(name):
    return re.search(r' d="([^"]+)"', open(os.path.join(HERE, name)).read()).group(1)


def place(d, width, cx, cy):
    """Scale a master path to `width` units with the beetle centred on (cx, cy)."""
    k = width / MASTER_W
    n = iter(range(10 ** 9))

    def num(m):
        v = float(m.group(0))
        v = (v - MASTER_CX) * k + cx if next(n) % 2 == 0 else (v - MASTER_CY) * k + cy
        return f'{v:.1f}'.rstrip('0').rstrip('.')

    out = [cmd + re.sub(r'-?\d+(?:\.\d+)?', num, args).strip() for cmd, args in re.findall(r'([MLCZ])([^MLCZ]*)', d)]
    return ' '.join(out).replace(' -', '-')


def svg(view, body):
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{view}" role="img" aria-label="Scarab">'
            f'<title>Scarab</title>{body}</svg>\n')


def path(d, fill, cls=''):
    c = f' class="{cls}"' if cls else ''
    return f'<path{c} fill="{fill}" fill-rule="evenodd" d="{d}"/>'


def outputs():
    A, F, M = master('armour.svg'), master('fill.svg'), master('mono.svg')
    tight = lambda d: place(d, 464, 240, 164)        # 480 x 328
    square = lambda d: place(d, 480, 256, 256)       # 512 square, for square slots
    small = lambda d: place(d, 504, 256, 256)        # 512 square, edge to edge, for 16 to 32 px
    tile = lambda d: place(d, 400, 256, 262)         # 512 square on an opaque background
    two = lambda f, a, ink, gold: path(f(a), ink) + path(f(F), gold)
    out = {
        'logo/scarab.svg': svg('0 0 480 328', two(tight, A, INK, GOLD)),
        'logo/scarab-dark.svg': svg('0 0 480 328', two(tight, A, CREAM, GOLD)),
        'logo/scarab-navy.svg': svg('0 0 480 328', two(tight, A, NAVY, GOLD)),
        'logo/scarab-mono-ink.svg': svg('0 0 480 328', path(tight(M), INK)),
        'logo/scarab-mono-cream.svg': svg('0 0 480 328', path(tight(M), CREAM)),
        'logo/scarab-square.svg': svg('0 0 512 512', two(square, A, INK, GOLD)),
        'logo/scarab-square-dark.svg': svg('0 0 512 512', two(square, A, CREAM, GOLD)),
        'logo/scarab-small-ink.svg': svg('0 0 512 512', path(small(M), INK)),
        'logo/scarab-small-cream.svg': svg('0 0 512 512', path(small(M), CREAM)),
        'logo/scarab-tile.svg': svg('0 0 512 512', f'<rect width="512" height="512" fill="{NIGHT}"/>' + two(tile, A, CREAM, GOLD)),
        'logo/scarab-small-tile.svg': svg('0 0 512 512', f'<rect width="512" height="512" fill="{NIGHT}"/>' + path(place(M, 440, 256, 262), CREAM)),
        # Browser tab icon: one colour, ink on a light tab and cream on a dark one.
        'favicon/favicon.svg': svg('0 0 512 512',
            '<style>path{fill:' + INK + '}@media (prefers-color-scheme:dark){path{fill:' + CREAM + '}}</style>'
            f'<path fill-rule="evenodd" d="{small(M)}"/>'),
    }
    return out


def build_ico():
    d = os.path.join(BRAND, 'favicon')
    sizes = [16, 32, 48, 64]
    pngs = [open(os.path.join(d, f'favicon-{s}.png'), 'rb').read() for s in sizes]
    hdr = struct.pack('<HHH', 0, 1, len(sizes)); off = 6 + 16 * len(sizes); ent = b''; data = b''
    for s, p in zip(sizes, pngs):
        ent += struct.pack('<BBBBHHII', s, s, 0, 0, 1, 32, len(p), off + len(data)); data += p
    open(os.path.join(d, 'favicon.ico'), 'wb').write(hdr + ent + data)
    print('wrote favicon/favicon.ico from', ', '.join(f'favicon-{s}.png' for s in sizes))


def main(argv):
    if '--ico' in argv:
        return build_ico()
    bad = 0
    for rel, text in outputs().items():
        dest = os.path.join(BRAND, rel)
        if '--check' in argv:
            same = os.path.exists(dest) and open(dest).read() == text
            print(('ok      ' if same else 'DIFFERS ') + rel); bad += not same
        else:
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            open(dest, 'w').write(text)
            print('wrote', rel, f'({len(text)} bytes)')
    return 1 if bad else 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
