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
// Character cells are 1:0.6 (e.g. 6×10 px), so scene y is squashed to keep
// circles round: rows = cols * (design height / design width) * 0.6.
export const CELL_ASPECT = 0.6;

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

// Dung-roller design space: 96 virtual units wide, squashed by CELL_ASPECT.
// The viewport starts at VB_Y0 (crops the empty sky above the ball).
const BEETLE_VB_W = 96;
const BEETLE_VB_Y0 = 26;
export const BEETLE_VB_H = 59;
const GY = 72;              // ground line
const BALL = { x: 63, r: 20 };
const LEG_CYCLES = 14;      // integer → seamless loop
const DOTS = 29;            // ground dots; spacing chosen so one loop = one wrap

/** Dung beetle pushing its ball — treadmill loop at phase u. */
export function drawBeetle(x, u, cols, rows) {
  const s = cols / BEETLE_VB_W;
  x.clearRect(0, 0, cols, rows);
  x.save();
  x.setTransform(s, 0, 0, s * CELL_ASPECT, 0, -BEETLE_VB_Y0 * s * CELL_ASPECT);
  x.lineCap = "round";

  // one ball revolution per loop; ground scrolls at the ball's surface speed
  const roll = u * TAU;
  const travel = roll * BALL.r;              // 2πr per loop
  const spacing = (TAU * BALL.r) / DOTS;     // pattern wraps exactly once
  x.fillStyle = "#57685e";
  for (let i = 0; i < DOTS + 6; i++) {
    const gx = ((i * spacing - travel) % (DOTS * spacing) + DOTS * spacing) % (DOTS * spacing) - spacing * 3;
    x.fillRect(gx, GY + 1.2, 1.5, 1.5);
  }

  // dung ball — outline + rotating flecks, resting on the ground
  const by = GY - BALL.r;
  x.strokeStyle = "#d9b45e"; x.lineWidth = 2.2;
  x.beginPath(); x.arc(BALL.x, by, BALL.r, 0, 7); x.stroke();
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

  x.strokeStyle = "#8fa89b"; x.lineWidth = 1.8;
  const ph1 = Math.sin(leg), ph2 = Math.sin(leg + Math.PI);
  x.beginPath(); x.moveTo(23, 67 + bob); x.quadraticCurveTo(20, 70, 18 + ph1 * 2.4, GY); x.stroke();
  x.beginPath(); x.moveTo(27, 68 + bob); x.quadraticCurveTo(26, 70.5, 25 + ph2 * 2.4, GY); x.stroke();
  x.beginPath(); x.moveTo(32, 68 + bob); x.quadraticCurveTo(32.5, 70.5, 32 + ph1 * 1.8, GY); x.stroke();
  x.beginPath(); x.moveTo(37, 57 + bob); x.quadraticCurveTo(43, 52.5, 46.5, 48 + ph2 * 1.2); x.stroke();
  x.beginPath(); x.moveTo(38.5, 61 + bob); x.quadraticCurveTo(44.5, 58.5, 48.5, 56 + ph1 * 1.2); x.stroke();
  x.restore();
}
