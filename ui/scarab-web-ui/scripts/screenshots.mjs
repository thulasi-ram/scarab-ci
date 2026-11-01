// The committed UI screenshots, captured from the mock UI (src/mock.ts, the
// "acme" fixture) with Playwright. `just docs screenshots` runs it.
//
//   node scripts/screenshots.mjs           # capture: overwrite every committed screenshot
//   node scripts/screenshots.mjs check     # compare a fresh capture with the committed files
//
// SHOTS lists every screenshot the README and the docs site show, at the size
// it is committed in. Every shot uses the same look (THEME and PALETTE below). One capture can be committed at several paths
// (the README and the docs site share two). `check` writes the fresh capture
// and a diff image per file to target/release-screenshots/ (or $SCREENSHOT_OUT)
// and exits 1 when any file drifted: a different size, or more than
// MAX_RATIO of its pixels differing by more than CHANNEL_DELTA in a channel.
// The tolerance absorbs JPEG re-encoding and font antialiasing, not UI changes.
//
// The clock is fixed, so "6m ago" and running durations render the same on
// every capture. The mock's `?theme=` and `?palette=` pin the look.
import { chromium } from "@playwright/test";
import { spawn } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const ui = resolve(here, "..");
const repo = resolve(ui, "../..");

const PORT = Number(process.env.SCREENSHOT_PORT ?? 4174);
const BASE = `http://127.0.0.1:${PORT}`;
const CHANNEL_DELTA = Number(process.env.SCREENSHOT_CHANNEL_DELTA ?? 48);
const MAX_RATIO = Number(process.env.SCREENSHOT_MAX_RATIO ?? 0.002);
const QUALITY = 80;
const CLOCK = new Date("2026-07-22T12:00:00Z");

// The docs show the UI in the light Mono palette. Every capture pins both.
const THEME = "light";
const PALETTE = "mono";

// The run the README's run view shows (RUN_ID in src/mock.ts).
const RUN_ID = "0190f8a2000071fb8c0011223344aabb";

const SHOTS = [
  {
    name: "dashboard",
    path: "/",
    size: [1485, 812],
    files: ["docs/assets/screenshots/dashboard-dark.jpg", "ui/scarab-docs-ui/src/assets/dashboard.jpg"],
  },
  {
    name: "run-detail",
    path: `/acme/scarab/runs/${RUN_ID}`,
    size: [1485, 812],
    files: ["docs/assets/screenshots/run-detail-dark.jpg", "ui/scarab-docs-ui/src/assets/run-detail.jpg"],
  },
  {
    // usage/environments.mdx: the New environment dialog (org Settings,
    // Environments section).
    name: "environments",
    path: "/orgs/acme/settings/environments",
    size: [1485, 812],
    files: ["ui/scarab-docs-ui/src/assets/environments.jpg"],
    async act(page) {
      await page.getByRole("button", { name: "New environment" }).click();
      await page.getByPlaceholder("production, staging, prod-eu").fill("production-eu");
    },
  },
  {
    // usage/secrets.mdx: the New secret dialog on a repository's Secrets tab.
    name: "secrets",
    path: "/acme/scarab?tab=secrets",
    size: [1562, 784],
    files: ["ui/scarab-docs-ui/src/assets/secrets.jpg"],
    async act(page) {
      await page.getByRole("button", { name: "New secret" }).first().click();
      await page.getByPlaceholder("NAME").fill("NPM_PUBLISH_TOKEN");
      await page.getByPlaceholder("value (write-only)").fill("x".repeat(24));
    },
  },
];

function startServer() {
  const child = spawn("npm", ["run", "dev:mock", "--", "--host", "127.0.0.1", "--port", String(PORT), "--strictPort"], {
    cwd: ui,
    detached: true,
    stdio: ["ignore", "ignore", "inherit"],
  });
  const stop = () => {
    try {
      process.kill(-child.pid, "SIGTERM");
    } catch {
      /* already gone */
    }
  };
  process.on("exit", stop);
  return { child, stop };
}

async function waitForServer(child) {
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`the mock UI server exited (${child.exitCode})`);
    try {
      const res = await fetch(BASE);
      if (res.ok) return;
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`the mock UI server did not answer on ${BASE} within 60 s`);
}

async function capture(browser, shot) {
  const [width, height] = shot.size;
  const context = await browser.newContext({
    viewport: { width, height },
    deviceScaleFactor: 1,
    colorScheme: THEME,
    reducedMotion: "reduce",
  });
  const page = await context.newPage();
  await page.clock.setFixedTime(CLOCK);
  const sep = shot.path.includes("?") ? "&" : "?";
  await page.goto(`${BASE}${shot.path}${sep}theme=${THEME}&palette=${PALETTE}`, { waitUntil: "networkidle" });
  await page.evaluate(() => document.fonts.ready);
  if (shot.act) await shot.act(page);
  await page.waitForTimeout(500);
  const jpeg = await page.screenshot({ type: "jpeg", quality: QUALITY, animations: "disabled" });
  await context.close();
  return jpeg;
}

// Decode both JPEGs in the browser (no image library needed) and count the
// pixels whose largest channel difference exceeds CHANNEL_DELTA. Returns the
// count, the sizes, and a PNG that paints those pixels red over a dimmed copy
// of the committed image.
async function compare(page, committed, fresh) {
  return page.evaluate(
    async ({ a, b, delta }) => {
      const load = async (b64) => {
        const img = new Image();
        img.src = `data:image/jpeg;base64,${b64}`;
        await img.decode();
        return img;
      };
      const [ia, ib] = [await load(a), await load(b)];
      const sizes = [
        [ia.naturalWidth, ia.naturalHeight],
        [ib.naturalWidth, ib.naturalHeight],
      ];
      if (sizes[0][0] !== sizes[1][0] || sizes[0][1] !== sizes[1][1]) {
        return { sizes, differing: -1, total: 0, diff: null };
      }
      const [w, h] = sizes[0];
      const pixels = (img) => {
        const c = document.createElement("canvas");
        c.width = w;
        c.height = h;
        const ctx = c.getContext("2d");
        ctx.drawImage(img, 0, 0);
        return ctx.getImageData(0, 0, w, h);
      };
      const pa = pixels(ia).data;
      const pb = pixels(ib).data;
      const out = document.createElement("canvas");
      out.width = w;
      out.height = h;
      const octx = out.getContext("2d");
      const od = octx.createImageData(w, h);
      let differing = 0;
      for (let i = 0; i < pa.length; i += 4) {
        const d = Math.max(
          Math.abs(pa[i] - pb[i]),
          Math.abs(pa[i + 1] - pb[i + 1]),
          Math.abs(pa[i + 2] - pb[i + 2]),
        );
        if (d > delta) {
          differing++;
          od.data.set([255, 0, 0, 255], i);
        } else {
          od.data.set([pa[i] >> 2, pa[i + 1] >> 2, pa[i + 2] >> 2, 255], i);
        }
      }
      octx.putImageData(od, 0, 0);
      return { sizes, differing, total: w * h, diff: out.toDataURL("image/png").split(",")[1] };
    },
    { a: committed.toString("base64"), b: fresh.toString("base64"), delta: CHANNEL_DELTA },
  );
}

async function main() {
  const mode = process.argv[2] ?? "capture";
  if (mode !== "capture" && mode !== "check") {
    console.error("usage: node scripts/screenshots.mjs [capture|check]");
    process.exit(2);
  }
  const outDir = resolve(process.env.SCREENSHOT_OUT ?? join(repo, "target", "release-screenshots"));
  const server = startServer();
  let browser;
  let drifted = 0;
  let files = 0;
  try {
    await waitForServer(server.child);
    browser = await chromium.launch();
    const scratch = await browser.newPage();
    if (mode === "check") mkdirSync(outDir, { recursive: true });
    for (const shot of SHOTS) {
      const fresh = await capture(browser, shot);
      if (mode === "capture") {
        for (const f of shot.files) {
          writeFileSync(join(repo, f), fresh);
          console.log(`wrote ${f} (${shot.size.join("x")}, ${THEME} ${PALETTE})`);
        }
        continue;
      }
      writeFileSync(join(outDir, `${shot.name}.jpg`), fresh);
      for (const f of shot.files) {
        files++;
        const path = join(repo, f);
        if (!existsSync(path)) {
          drifted++;
          console.log(`DRIFT ${f}: not committed`);
          continue;
        }
        const r = await compare(scratch, readFileSync(path), fresh);
        if (r.differing < 0) {
          drifted++;
          console.log(`DRIFT ${f}: committed ${r.sizes[0].join("x")}, capture ${r.sizes[1].join("x")}`);
          continue;
        }
        const ratio = r.differing / r.total;
        const pct = (ratio * 100).toFixed(2);
        if (ratio > MAX_RATIO) {
          drifted++;
          const diffName = `${f.replaceAll("/", "__")}.diff.png`;
          writeFileSync(join(outDir, diffName), Buffer.from(r.diff, "base64"));
          console.log(`DRIFT ${f}: ${pct}% of pixels differ (diff: ${join(outDir, diffName)})`);
        } else {
          console.log(`ok    ${f}: ${pct}% of pixels differ`);
        }
      }
    }
  } finally {
    await browser?.close();
    server.stop();
  }
  if (mode === "check") {
    console.log(`${drifted} of ${files} screenshots drifted; fresh captures in ${outDir}`);
    process.exit(drifted ? 1 : 0);
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(2);
});
