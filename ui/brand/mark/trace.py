#!/usr/bin/env python3
"""Trace the beetle artwork (source.png) into the two master paths of the mark.

    python3 ui/brand/mark/trace.py

Needs numpy, opencv-python-headless and pillow, plus the `potrace` binary.
`generate.py` (standard library only) builds every logo and favicon SVG from
the two masters this writes, so run this only when the artwork or the cuts
change:

  plates.svg  the mark: the silhouette with uniform cuts between the plates
  bold.svg    the small-size mark: the silhouette grown by BOLD units, no cuts,
              for 16 to 32 px

Steps: average the artwork with its mirror image so the two halves match
exactly, threshold the silhouette, draw the plate and joint cuts below (left
half, mirrored), trace with potrace, and write one absolute path per master in
a 320 x 480 box.
"""
import json, os, re, subprocess, sys, tempfile
import numpy as np, cv2
from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
AXIS_SRC = 439      # x of the beetle's axis in source.png
PAD = 60            # margin added around the artwork
F = 4               # supersampling for the trace
CUT = 8             # cut width, source pixels
BOLD = 6            # growth of the small-size mark, source pixels
SMOOTH = 2          # radius of the opening that smooths the outline, source pixels

# Cuts on the left half, in padded-canvas pixels (axis at x = 499). Each is
# mirrored to the right half.
CUTS = {
    'horn':    [[425, 410], [457, 487], [499, 508]],
    'lobe':    [[368, 450], [398, 556], [462, 620], [482, 627], [499, 627]],
    'elytra':  [[300, 622], [380, 608], [445, 642], [499, 700]],
    'suture':  [[499, 700], [499, 1060]],
    'strip':   [[310, 680], [362, 742], [380, 852], [300, 935]],
    'panel':   [[380, 852], [460, 940], [482, 1012], [499, 1040]],
    'fl_claw': [[230, 323], [285, 333]],
    'fl_knee': [[245, 505], [296, 482]],
    'ml_claw': [[150, 782], [210, 790]],
    'ml_knee': [[245, 632], [258, 695]],
    'hl_knee': [[225, 852], [280, 880]],
    'hl_claw': [[268, 1042], [305, 1022]],
}

def keep_large(m, min_area):
    n, lab, st, _ = cv2.connectedComponentsWithStats(m.astype(np.uint8), 8)
    keep = np.zeros(n, bool); keep[1:] = st[1:, cv2.CC_STAT_AREA] >= min_area
    return keep[lab]

def silhouette():
    g = np.array(Image.open(os.path.join(HERE, 'source.png')).convert('L')).astype(np.float32)
    h, w = g.shape
    cw = 2 * max(AXIS_SRC, w - AXIS_SRC) + 2 * PAD
    canvas = np.full((h + 2 * PAD, cw), 255, np.float32)
    off = cw // 2 - AXIS_SRC
    canvas[PAD:PAD + h, off:off + w] = g
    sym = (canvas + canvas[:, ::-1]) / 2                     # exact mirror symmetry
    up = cv2.resize(sym, None, fx=F, fy=F, interpolation=cv2.INTER_CUBIC)
    up = cv2.GaussianBlur(up, (0, 0), 3)
    sil = keep_large(up < 150, 4000)
    sil = ~keep_large(~sil, 800)
    # Shave the knuckle bumps the painted outline left on the legs: an opening
    # removes anything narrower than 2 * SMOOTH source pixels.
    r = SMOOTH * F
    k = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (2 * r + 1, 2 * r + 1))
    sil = cv2.morphologyEx(sil.astype(np.uint8), cv2.MORPH_OPEN, k) > 0
    return sil, cw / 2

def trace(mask):
    with tempfile.TemporaryDirectory() as t:
        pbm, out = os.path.join(t, 'm.pbm'), os.path.join(t, 'm.svg')
        Image.fromarray(np.where(mask, 0, 255).astype(np.uint8)).save(pbm)
        subprocess.run(['potrace', '-s', '--flat', '-a', '1.0', '-O', '1.2', '-t', '40', '-u', '4', pbm, '-o', out], check=True)
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
        if cmd.islower():
            v = [v[k] + (cx if k % 2 == 0 else cy) for k in range(n)]
        cx, cy = v[-2], v[-1]
        if cmd in 'Mm': sx, sy = cx, cy; segs.append(['M'] + v); cmd = 'L' if cmd == 'M' else 'l'
        else: segs.append([cmd.upper()] + v)
    # potrace units -> canvas pixels (y flipped)
    return [[s[0]] + [s[k] * sc / F if k % 2 == 1 else (vb_h - s[k] * sc) / F for k in range(1, len(s))] for s in segs]

def write_master(name, segs, axis):
    ys = [s[k] for s in segs for k in range(2, len(s), 2)]
    y0, y1 = min(ys), max(ys)
    s = 464 / (y1 - y0)                                   # 464 tall in a 320 x 480 box, centred
    tx, ty = 160 - axis * s, 240 - (y0 + y1) / 2 * s
    fmt = lambda v: f'{v:.2f}'.rstrip('0').rstrip('.')
    parts = []
    for seg in segs:
        if seg[0] == 'Z': parts.append('Z'); continue
        pts = [fmt(seg[k] * s + (tx if k % 2 == 1 else ty)) for k in range(1, len(seg))]
        parts.append(seg[0] + ' '.join(pts))
    d = ' '.join(parts).replace(' -', '-')
    svg = ('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 320 480">'
           f'<path fill-rule="evenodd" d="{d}"/></svg>\n')
    open(os.path.join(HERE, name), 'w').write(svg)
    print(f'wrote mark/{name} ({len(svg)} bytes)')

def main():
    sil, axis = silhouette()
    cut = np.zeros(sil.shape, np.uint8)
    for pts in CUTS.values():
        for side in (1, -1):
            p = np.array([[(axis + side * (x - axis)) * F, y * F] for x, y in pts], np.float32)
            cv2.polylines(cut, [np.round(p * 16).astype(np.int32)], False, 255, CUT * F, cv2.LINE_AA, shift=4)
    write_master('plates.svg', trace(keep_large(sil & (cut < 128), 3000)), axis)
    r = BOLD * F
    bold = cv2.dilate(sil.astype(np.uint8), cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (2 * r + 1, 2 * r + 1))) > 0
    write_master('bold.svg', trace(bold), axis)

if __name__ == '__main__':
    main()
