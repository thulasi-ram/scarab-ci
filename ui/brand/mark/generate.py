#!/usr/bin/env python3
"""Generate the Scarab mark's logo and favicon SVGs from the two traced masters.

    python3 ui/brand/mark/generate.py           write the SVGs
    python3 ui/brand/mark/generate.py --check   fail if the SVGs on disk differ
    python3 ui/brand/mark/generate.py --ico     build favicon.ico from the favicon PNGs

Standard library only. The masters, `plates.svg` (the mark) and `bold.svg`
(the small-size mark), are written by trace.py from source.png. Each holds one
absolute path in a 320 x 480 box with the beetle centred at (160, 240) and 464
units tall. This script scales that path into each output; the PNGs are then
rendered from the outputs by serve_png.py.
"""
import os
import re
import struct
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
BRAND = os.path.realpath(os.path.join(HERE, '..'))

INK, CREAM, EMERALD, GOLD, NIGHT = '#111418', '#f4f1ea', '#1f9e74', '#c9a94e', '#0b0c0e'
MASTER_H = 464


def master(name):
    text = open(os.path.join(HERE, name)).read()
    return re.search(r' d="([^"]+)"', text).group(1)


def place(d, height, cx, cy):
    """Scale a master path to `height` units with the beetle centred on (cx, cy)."""
    k = height / MASTER_H
    nums = iter(range(10 ** 9))

    def num(m):
        v = float(m.group(0))
        v = (v - 160) * k + cx if next(nums) % 2 == 0 else (v - 240) * k + cy
        return f'{v:.1f}'.rstrip('0').rstrip('.')

    out = []
    for cmd, args in re.findall(r'([MLCZ])([^MLCZ]*)', d):
        out.append(cmd + re.sub(r'-?\d+(?:\.\d+)?', num, args).strip())
    return ' '.join(out).replace(' -', '-')


def svg(view, body):
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{view}" role="img" aria-label="Scarab">'
            f'<title>Scarab</title>{body}</svg>\n')


def path(d, fill, cls=''):
    c = f' class="{cls}"' if cls else ''
    return f'<path{c} fill="{fill}" fill-rule="evenodd" d="{d}"/>'


def outputs():
    plates, bold = master('plates.svg'), master('bold.svg')
    tight = place(plates, 464, 160, 240)        # 320 x 480, a little margin round the beetle
    square = place(plates, 480, 256, 256)       # 512 square
    tile = place(plates, 392, 256, 256)         # 512 square on an opaque background
    small = place(bold, 496, 256, 256)          # 512 square, for 16 to 32 px
    small_tile = place(bold, 432, 256, 256)
    out = {}
    for name, col in (('ink', INK), ('cream', CREAM), ('emerald', EMERALD), ('gold', GOLD)):
        out[f'logo/scarab-{name}.svg'] = svg('0 0 320 480', path(tight, col))
        out[f'logo/scarab-square-{name}.svg'] = svg('0 0 512 512', path(square, col))
        out[f'logo/scarab-small-{name}.svg'] = svg('0 0 512 512', path(small, col))
    out['logo/scarab-tile.svg'] = svg('0 0 512 512', f'<rect width="512" height="512" fill="{NIGHT}"/>' + path(tile, CREAM))
    out['logo/scarab-tile-emerald.svg'] = svg('0 0 512 512', f'<rect width="512" height="512" fill="{EMERALD}"/>' + path(tile, INK))
    out['logo/scarab-small-tile.svg'] = svg('0 0 512 512', f'<rect width="512" height="512" fill="{NIGHT}"/>' + path(small_tile, CREAM))
    out['logo/scarab-small-tile-emerald.svg'] = svg('0 0 512 512', f'<rect width="512" height="512" fill="{EMERALD}"/>' + path(small_tile, INK))
    # Browser tab icon: the small-size mark, ink on a light tab and cream on a dark one.
    out['favicon/favicon.svg'] = svg('0 0 512 512',
        '<style>path{fill:' + INK + '}@media (prefers-color-scheme:dark){path{fill:' + CREAM + '}}</style>'
        f'<path fill-rule="evenodd" d="{small}"/>')
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
