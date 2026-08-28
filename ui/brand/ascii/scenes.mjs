// Brand beetle scenes — parametric canvas-2D drawings baked to ASCII by bake.mjs.
//
// Every scene draws in "view units" onto a tiny sampler canvas (1 px = 1 cell)
// and takes a loop phase u ∈ [0,1). All periodic motion is an integer function
// of u, so frame N-1 → frame 0 is seamless by construction.
//
// The scarab is a parametric rebuild at the traced emblem's measured
// proportions (../logo/scarab-emblem.svg, viewBox 2196×1952): the traced dark
// layer is one winding-linked compound path — its background whites are hole
// contours — so it can't be dismembered for wing articulation. Measured
// geometry: ring r≈894 centered (1089,950); wing pivots (882,1030)/(1314,1030);
// upper blade 445 long at 139° up-out, lower blade 372 at 42° down-out.
//
// Fill colors here are ROLE TAGS, not display colors: bake.mjs classifies each
// cell as emerald / gold / gray by hue and the players color those layers with
// CSS. Keep fills in these three hue families.

export const VB_W = 2196;
export const VB_H = 1952;
// Cells render SQUARE — the players set line-height to the 0.6em glyph advance,
// so scene y is NOT squashed and the dot matrix is evenly spaced on both axes
// (one language with the dot-matrix doodles). rows = cols * (VBh / VBw).
export const CELL_ASPECT = 1.0;

const RING = { cx: 1089, cy: 950, r: 894, w: 42 };
const PIVOT_L = { x: 882, y: 1030 };
const PIVOT_R = { x: 1314, y: 1030 };
const UPPER = { len: 445, hw: 92, angL: (-139.2 * Math.PI) / 180 };
const LOWER = { len: 372, hw: 62, angL: (138.3 * Math.PI) / 180 };

const TAU = Math.PI * 2;

function leaf(x, len, hw) {
  x.beginPath();
  x.moveTo(0, 0);
  x.bezierCurveTo(len * 0.25, -hw, len * 0.85, -hw * 0.75, len, 0);
  x.bezierCurveTo(len * 0.85, hw * 0.75, len * 0.25, hw, 0, 0);
  x.fill();
}

function easeOutBack(t) {
  const c = 1.70158, u = t - 1;
  return 1 + (c + 1) * u * u * u + c * u * u;
}
function easeInOut(t) {
  return t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;
}

// Loop phase -> wing spread (with overshoot) + halo glow.
function loopState(u) {
  let spread;
  if (u < 0.1) spread = 0;
  else if (u < 0.38) spread = easeOutBack((u - 0.1) / 0.28);
  else if (u < 0.86) spread = 1 + 0.05 * Math.sin(((u - 0.38) / 0.48) * Math.PI * 4);
  else spread = 1 - easeInOut((u - 0.86) / 0.14);
  const open = Math.max(0, Math.min(1, spread));
  const glow = open * (0.55 + 0.45 * Math.sin(u * TAU * 3));
  return { spread, glow, open };
}

/** Wing-spread + nimbus. Draws on a cols×rows sampler at phase u. */
export function drawScarab(x, u, cols, rows) {
  const { spread, glow, open } = loopState(u);
  const sx = cols / VB_W;
  const sy = sx * CELL_ASPECT;
  x.clearRect(0, 0, cols, rows);
  x.save();
  x.setTransform(sx, 0, 0, sy, 0, (rows - VB_H * sy) / 2);
  x.lineCap = "round";

  // nimbus ring + pulse
  x.strokeStyle = `rgba(201,169,78,${(0.42 + 0.25 * glow).toFixed(3)})`;
  x.lineWidth = RING.w;
  x.beginPath(); x.arc(RING.cx, RING.cy, RING.r, 0, 7); x.stroke();
  if (glow > 0.05) {
    x.strokeStyle = `rgba(201,169,78,${(0.16 * glow).toFixed(3)})`;
    x.lineWidth = RING.w * 2.6;
    x.beginPath(); x.arc(RING.cx, RING.cy, RING.r - RING.w * 1.6, 0, 7); x.stroke();
  }

  // wing films — only visible when open; clipped to the ring interior
  if (open > 0.02) {
    x.strokeStyle = `rgba(90,128,113,${(0.85 * open * open).toFixed(3)})`;
    x.lineWidth = 14;
    const R2 = RING.r * 0.93;
    for (const side of [-1, 1]) {
      const pv = side < 0 ? PIVOT_L : PIVOT_R;
      for (const a of [-2.42, -1.95, Math.PI]) {
        const ang = side < 0 ? a : Math.PI - a;
        const dx = Math.cos(ang), dy = Math.sin(ang);
        const px = pv.x - RING.cx, py = pv.y - RING.cy;
        const b = dx * px + dy * py;
        const t = -b + Math.sqrt(Math.max(0, b * b - (px * px + py * py - R2 * R2)));
        x.beginPath(); x.moveTo(pv.x, pv.y); x.lineTo(pv.x + dx * t, pv.y + dy * t); x.stroke();
      }
    }
  }

  // wings — a rotation around the shoulder pivots; closed tucks along the body
  const foldU = (1 - spread) * 2.0;
  const foldL = (1 - spread) * 0.62;
  x.fillStyle = "#27b584";
  for (const side of [-1, 1]) {
    const pv = side < 0 ? PIVOT_L : PIVOT_R;
    const fU = side < 0 ? UPPER.angL - foldU : Math.PI - UPPER.angL + foldU;
    const fL = side < 0 ? LOWER.angL - foldL : Math.PI - LOWER.angL + foldL;
    x.save(); x.translate(pv.x, pv.y); x.rotate(fU); leaf(x, UPPER.len, UPPER.hw); x.restore();
    x.save(); x.translate(pv.x, pv.y); x.rotate(fL); leaf(x, LOWER.len, LOWER.hw); x.restore();
  }

  // legs + antennae (static, dim)
  x.strokeStyle = "#6e7f76";
  x.lineWidth = 24;
  for (const s of [-1, 1]) {
    x.save(); x.translate(RING.cx, 0); x.scale(s, 1); x.translate(-RING.cx, 0);
    x.beginPath(); x.moveTo(985, 840); x.bezierCurveTo(880, 770, 850, 690, 880, 590); x.stroke();
    x.beginPath(); x.moveTo(905, 1080); x.bezierCurveTo(790, 1075, 730, 1040, 690, 1000); x.stroke();
    x.beginPath(); x.moveTo(950, 1330); x.bezierCurveTo(860, 1430, 800, 1500, 790, 1580); x.stroke();
    x.beginPath(); x.moveTo(1045, 690); x.bezierCurveTo(1000, 620, 970, 580, 965, 540); x.stroke();
    x.restore();
  }

  // body — gold, drawn last so it caps the wing roots
  x.fillStyle = "#d9b45e";
  x.beginPath(); x.ellipse(1089, 724, 104, 50, 0, 0, 7); x.fill();
  x.beginPath(); x.ellipse(1089, 886, 222, 112, 0, 0, 7); x.fill();
  x.beginPath(); x.ellipse(1089, 1242, 224, 234, 0, 0, 7); x.fill();
  // grooves (background-colored: they carve the body in every theme)
  x.strokeStyle = "#000000"; x.lineWidth = 18;
  x.globalCompositeOperation = "destination-out";
  x.beginPath(); x.moveTo(1089, 1015); x.lineTo(1089, 1468); x.stroke();
  x.beginPath(); x.moveTo(870, 1002); x.lineTo(1308, 1002); x.stroke();
  x.beginPath(); x.moveTo(1020, 1015); x.lineTo(1089, 1105); x.lineTo(1158, 1015); x.stroke();
  x.globalCompositeOperation = "source-over";

  x.restore();
}

// Dung-roller design space: 96 virtual units wide (CELL_ASPECT is 1.0: square).
// The viewport starts at VB_Y0 (crops the empty sky above the ball).
const BEETLE_VB_W = 96;
const BEETLE_VB_Y0 = 26;
export const BEETLE_VB_H = 59;       // with ground dots (treadmill variant)
export const BEETLE_VB_H_BARE = 48;  // cropped at the feet (traveling variant)
const GY = 72;              // ground line
const BALL = { x: 63, r: 20 };

// Limb style, shared by EVERY beetle (DUNG.md §1 + §6). One definition, because
// the two scenes had drifted to 1.65 cells / #8fa89b here and 1.10 cells /
// #6e7f76 in the Ponderer — where the legs all but vanished. Two things set a
// limb's weight, and both live here:
//
//   • the stroke, in CELLS — `LEG.cells / s`, never a raw design-unit width,
//     which means something different in every scene's design space;
//   • the COLOUR, because the bake maps luminance to dot SIZE on the " .·•●"
//     ramp. A lighter stroke crosses each ramp step at lower coverage, so it
//     bakes to bigger dots. Darkening a limb thins it as surely as narrowing it.
//
// 1.65 cells is the dung-roller's historical 1.8 design units (its s = 88/96),
// so adopting this constant leaves the roller's bake byte-identical.
export const LEG = { color: "#8fa89b", cells: 1.65 };
const LEG_CYCLES = 14;      // integer → seamless loop
const DOTS = 29;            // ground dots; spacing chosen so one loop = one wrap

/** Dung beetle pushing its ball at phase u.
 *  opts.ground=true  → treadmill: beetle in place, ground dots scroll past.
 *  opts.ground=false → traveling: no ground (the host moves the whole scene —
 *  e.g. the web-ui footer walks it across the viewport on a CSS animation). */
export function drawBeetle(x, u, cols, rows, opts = {}) {
  const { ground = true } = opts;
  const s = cols / BEETLE_VB_W;
  x.clearRect(0, 0, cols, rows);
  x.save();
  x.setTransform(s, 0, 0, s * CELL_ASPECT, 0, -BEETLE_VB_Y0 * s * CELL_ASPECT);
  x.lineCap = "round";

  // one ball revolution per loop; ground scrolls at the ball's surface speed
  const roll = u * TAU;
  if (ground) {
    const travel = roll * BALL.r;              // 2πr per loop
    const spacing = (TAU * BALL.r) / DOTS;     // pattern wraps exactly once
    x.fillStyle = "#57685e";
    for (let i = 0; i < DOTS + 6; i++) {
      const gx = ((i * spacing - travel) % (DOTS * spacing) + DOTS * spacing) % (DOTS * spacing) - spacing * 3;
      x.fillRect(gx, GY + 1.2, 1.5, 1.5);
    }
  }

  // dung ball — outline + rotating flecks, resting on the ground.
  // The rim is dashed and phase-locked to the roll (a smooth circle rotating
  // is invisible); 21 segments divide the circumference exactly, so the loop
  // stays seamless.
  const by = GY - BALL.r;
  const rimPeriod = (TAU * BALL.r) / 21;
  x.strokeStyle = "#d9b45e"; x.lineWidth = 2.2;
  x.setLineDash([rimPeriod * 0.62, rimPeriod * 0.38]);
  x.lineDashOffset = -roll * BALL.r;
  x.beginPath(); x.arc(BALL.x, by, BALL.r, 0, 7); x.stroke();
  x.setLineDash([]);
  x.fillStyle = "#b3924a";
  for (let i = 0; i < 16; i++) {
    const a = roll + i * 2.39996;            // golden-angle scatter
    const rr = BALL.r * (0.15 + 0.75 * ((i * 0.61) % 1));
    const fs = 1.4 + ((i * 7) % 3) * 0.8;
    x.beginPath(); x.arc(BALL.x + Math.cos(a) * rr, by + Math.sin(a) * rr, fs, 0, 7); x.fill();
  }

  // beetle, head-down at the ground, hind legs up on the ball
  // (dung beetles really do push walking backwards)
  const leg = u * TAU * LEG_CYCLES;
  const bob = Math.sin(leg) * 0.8;
  x.save();
  x.translate(30, 61 + bob); x.rotate(-0.62);
  x.fillStyle = "#27b584";
  x.beginPath(); x.ellipse(0, 0, 10.5, 6.5, 0, 0, 7); x.fill();
  x.fillStyle = "#1c8a64";
  x.beginPath(); x.ellipse(-10.5, 2, 3.6, 3, 0, 0, 7); x.fill();
  x.restore();

  x.strokeStyle = LEG.color; x.lineWidth = LEG.cells / s;
  const ph1 = Math.sin(leg), ph2 = Math.sin(leg + Math.PI);
  x.beginPath(); x.moveTo(23, 67 + bob); x.quadraticCurveTo(20, 70, 18 + ph1 * 2.4, GY); x.stroke();
  x.beginPath(); x.moveTo(27, 68 + bob); x.quadraticCurveTo(26, 70.5, 25 + ph2 * 2.4, GY); x.stroke();
  x.beginPath(); x.moveTo(32, 68 + bob); x.quadraticCurveTo(32.5, 70.5, 32 + ph1 * 1.8, GY); x.stroke();
  x.beginPath(); x.moveTo(37, 57 + bob); x.quadraticCurveTo(43, 52.5, 46.5, 48 + ph2 * 1.2); x.stroke();
  x.beginPath(); x.moveTo(38.5, 61 + bob); x.quadraticCurveTo(44.5, 58.5, 48.5, 56 + ph1 * 1.2); x.stroke();
  x.restore();
}

// ── The Ponderer: a reusable "bubble stage" ─────────────────────────────────
// A treadmill beetle that rolls in, HOLDS a pose for the middle of the loop,
// then rolls on. The held pose is swappable (ponder / nap / kingofhill /
// faceplant); each exposes an anchor where a runtime speech bubble points.
// The bubble TEXT is never baked — bake.mjs records only {from,to,col,row,place}
// and the players composite a box around a `line` prop at those frames. Same
// three role layers as every scene; the bubble is a fourth, runtime-only layer.
export const PONDER = {
  W: 74, VBy0: 10, VBh: 33, gy: 40, r: 12, ballX: 48, bx: 28,
  R0: 0.30, R1: 0.78, // roll-in ends at R0; hold ends (roll-out begins) at R1
};
export const PONDER_POSES = ["ponder", "nap", "kingofhill", "faceplant"];

// Trapezoid settle: ease into the pose, hold it, ease back out over the hold.
function envelope(p) {
  const up = Math.min(1, p / 0.18);
  const dn = Math.min(1, (1 - p) / 0.18);
  const e = Math.min(up, dn);
  return e < 1 ? easeInOut(e) : 1;
}

// Pose deltas at hold-amount a ∈ [0,1] (a=0 is the push pose, a=1 fully posed).
function ponderPose(pose, a) {
  const bx0 = PONDER.bx, by0 = PONDER.gy - 4; // push pose baseline
  switch (pose) {
    case "nap":        // slumps back against the ball, eyes shut
      return { bx: bx0 + 5 * a, by: by0 + 0.5 * a, tilt: -0.5 + 0.35 * a, grip: 1 - a, ballDX: 0, onBall: false, eyes: "shut" };
    case "kingofhill": // climbs onto the ball and sits, surveying
      return { bx: bx0 + 20 * a, by: by0 - 22 * a, tilt: -0.5 + 0.5 * a, grip: 1 - a, ballDX: 0, onBall: a > 0.5, eyes: "open" };
    case "faceplant":  // over-commits and tips onto its face; ball creeps on
      return { bx: bx0 + 6 * a, by: by0 + 2.5 * a, tilt: -0.5 - 1.1 * a, grip: 1 - a, ballDX: 3 * a, onBall: false, eyes: "dazed" };
    case "ponder":
    default:           // leans back on its haunches, looks up
      return { bx: bx0, by: by0 - 1.2 * a, tilt: -0.5 + 0.9 * a, grip: 1 - 0.9 * a, ballDX: 0, onBall: false, eyes: "open" };
  }
}

/** World-space point the bubble tail points at, + placement side. */
export function ponderAnchor(pose) {
  const q = ponderPose(pose, 1);
  if (pose === "kingofhill") return { x: q.bx + 10, y: q.by + 1, place: "right" };
  return { x: q.bx - 7, y: q.by - 4, place: "above" };
}

function ponderBall(x, cx, cy, r, roll, s) {
  // The ball follows DUNG.md — the spec every scene's ball shares. Two rules
  // are load-bearing here:
  //
  // 1. The rim is measured in CELLS, not design units, exactly like the legs
  //    below. This stage is a 74-unit space baked to 96 cells (s ≈ 1.30), so a
  //    2-unit rim landed at 2.6 cells while the dungroller's 2.2-unit rim in
  //    its 96-unit space baked to 88 cells is 2.0 — the Ponderer's ball was
  //    drawn 28% heavier on a ball 16% smaller, and read as a thick doughnut
  //    beside the roller's sleek ring.
  // 2. The rim is DASHED and phase-locked to the roll, because a smooth circle
  //    rotating is invisible. This reverses an earlier note here claiming a
  //    dashed rim "reads as a *broken* ball" — true at the old 64 grid, false
  //    at 96, where the dashes land at 2.89 cells (DUNG.md §3 carries the
  //    ≥2.5-cell threshold and the comparison that settled it).
  x.strokeStyle = "#d9b45e"; x.lineWidth = 2 / s;
  const rimPeriod = (TAU * r) / 21;
  x.setLineDash([rimPeriod * 0.62, rimPeriod * 0.38]);
  x.lineDashOffset = -roll * r;
  x.beginPath(); x.arc(cx, cy, r, 0, 7); x.stroke();
  x.setLineDash([]);
  // Sparse, uniform flecks — dung texture that rotates with the roll, kept few
  // and small so they read as texture rather than a swirl inside the ball.
  x.fillStyle = "#b3924a";
  for (let i = 0; i < 9; i++) {
    const a = roll + i * 2.39996;
    const rr = r * (0.22 + 0.48 * ((i * 0.61) % 1));
    const fs = r * 0.085;
    x.beginPath(); x.arc(cx + Math.cos(a) * rr, cy + Math.sin(a) * rr, fs, 0, 7); x.fill();
  }
}

function ponderGround(x, gy, travel, r) {
  const DOTS = 22;
  const spacing = (TAU * r) / DOTS;
  x.fillStyle = "#57685e";
  for (let i = 0; i < DOTS + 8; i++) {
    const gx = ((i * spacing - travel) % (DOTS * spacing) + DOTS * spacing) % (DOTS * spacing) - spacing * 4;
    x.fillRect(gx, gy + 1.4, 1.3, 1.3);
  }
}

function ponderBeetle(x, bx, by, gy, tilt, grip, legPh, ballCx, ballTop, ballR, s, onBall) {
  const R = 6.6;
  // Limbs are the roller's, exactly — see LEG. The geometry below cannot be
  // shared with it (this beetle translates, tilts, lets go of the ball and can
  // climb onto it; the roller's legs are constants for a beetle that does none
  // of those), but the STYLE is not geometry and had no business differing.
  x.strokeStyle = LEG.color; x.lineWidth = LEG.cells / s;
  const ph1 = Math.sin(legPh), ph2 = Math.sin(legPh + Math.PI);
  if (onBall) {
    // legs tucked onto the ball crown instead of the ground
    for (const dx of [-4, 0, 4]) {
      x.beginPath(); x.moveTo(bx + dx * 0.6, by + 2); x.quadraticCurveTo(bx + dx, by + 5, bx + dx, ballTop + 2); x.stroke();
    }
  } else {
    x.beginPath(); x.moveTo(bx - 5, by + 2); x.quadraticCurveTo(bx - 7, by + 5, bx - 8 + ph1 * 2.2, gy); x.stroke();
    x.beginPath(); x.moveTo(bx - 1, by + 3); x.quadraticCurveTo(bx - 1, by + 5.5, bx - 1 + ph2 * 2.2, gy); x.stroke();
    x.beginPath(); x.moveTo(bx + 3, by + 3); x.quadraticCurveTo(bx + 3.5, by + 5.5, bx + 3 + ph1 * 1.7, gy); x.stroke();
    // arms — reach to the ball's near SURFACE when gripping (never onto/over the
    // gold ball), else fold to the chest. The hands must STOP at the rim's outer
    // edge: the bake classifies each cell to exactly ONE layer, so a limb drawn
    // over the rim does not overlay it, it REPLACES those cells with gray and
    // punches a notch out of the gold ring (DUNG.md §4 says the same about a
    // fleck touching the rim from the inside).
    //
    // So the clearance is derived, not guessed: half the rim plus half the limb,
    // both of which are specified in cells, converted back to design units. A
    // hand-tuned `ballR * 1.04` put the tip at 12.48 units with the rim spanning
    // 11.23–12.77 — inside the band, which cost ~16 of the rim's 222 cells while
    // the beetle pushed.
    const ax = bx + 5.5, ay = by - 1;
    const clear = (2 / 2 + LEG.cells / 2) / s;
    const ballCy = ballTop + ballR, reach = ballR + clear;
    // Both hands land ON the clearance circle, at two angles on the upper-left
    // arc — never one hand placed and the other nudged off it by a hand-tuned
    // offset, which is how the lower arm ended up back inside the rim band.
    const hand = (ang) => {
      const hx = ballCx + reach * Math.cos(ang), hy = ballCy + reach * Math.sin(ang);
      return [ax + (hx - ax) * grip - (1 - grip) * 1.5, ay + (hy - ay) * grip + (1 - grip) * 3];
    };
    // Angles chosen by measurement, not by eye: the shoulder sits at ~154° from
    // the ball centre, so a hand placed high on the arc (the old ~200°) made the
    // arm cross the rim band OBLIQUELY — running along the ring instead of into
    // it, which is what maximises the cells it steals. Sweeping the pair over
    // the arc, 0.95π/1.03π leaves 9 limb cells in the band against 74 at
    // 1.11π/1.19π, and hands 65 more gold rim cells back.
    const [gx1, gy1] = hand(Math.PI * 0.95);   // ~171°
    const [gx2, gy2] = hand(Math.PI * 1.03);   // ~185°, the lower arm
    x.beginPath(); x.moveTo(ax, ay); x.quadraticCurveTo(ax + 1.5, ay - 2, gx1, gy1); x.stroke();
    x.beginPath(); x.moveTo(bx + 4, by + 1); x.quadraticCurveTo(ax + 1, ay + 1, gx2, gy2); x.stroke();
  }
  // body + head, rotated about (bx,by)
  x.save(); x.translate(bx, by); x.rotate(tilt);
  x.fillStyle = "#27b584";
  x.beginPath(); x.ellipse(0, 0, R, 4.3, 0, 0, 7); x.fill();
  x.strokeStyle = "#1c8a64"; x.lineWidth = 0.8;
  x.beginPath(); x.moveTo(-R * 0.4, -2.2); x.lineTo(R * 0.6, 0.4); x.stroke();
  x.fillStyle = "#1c8a64";
  x.beginPath(); x.ellipse(-R - 0.6, 1.3, 2.4, 2, 0, 0, 7); x.fill();
  x.strokeStyle = LEG.color; x.lineWidth = 0.8;
  x.beginPath(); x.moveTo(-R - 1.4, 0.2); x.quadraticCurveTo(-R - 3.5, -1.6, -R - 4, -3.2); x.stroke();
  x.restore();
}

/** Ponderer stage at phase u. opts.pose ∈ PONDER_POSES. */
export function drawPonderer(x, u, cols, rows, opts = {}) {
  const pose = opts.pose || "ponder";
  const P = PONDER;
  const s = cols / P.W;
  x.clearRect(0, 0, cols, rows);
  x.save();
  x.setTransform(s, 0, 0, s * CELL_ASPECT, 0, -P.VBy0 * s * CELL_ASPECT);
  x.lineCap = "round"; x.lineJoin = "round";

  const { R0, R1, r, gy, ballX } = P;
  // ground travel is flat during the hold, so total over the loop = one wrap
  const rollFrac = R0 + (1 - R1);
  const spd = (TAU * r) / rollFrac;
  let travel;
  if (u < R0) travel = spd * u;
  else if (u < R1) travel = spd * R0;
  else travel = spd * (R0 + (u - R1));
  const roll = travel / r;
  ponderGround(x, gy, travel, r);

  const a = (u >= R0 && u <= R1) ? envelope((u - R0) / (R1 - R0)) : 0;
  const q = ponderPose(pose, a);
  const ballCx = ballX + q.ballDX;
  const ballCy = gy - r;
  ponderBall(x, ballCx, ballCy, r, roll, s);

  const legPh = (u < R0 ? u : u < R1 ? R0 : u) * TAU * 13;
  ponderBeetle(x, q.bx, q.by, gy, q.tilt, q.grip, legPh, ballCx, ballCy - r, r, s, q.onBall);
  x.restore();
}
