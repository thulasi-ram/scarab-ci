import { describe, it, expect } from "vitest";
import {
  appendLog,
  backoffDelay,
  emptyLogBuffer,
  followLog,
  formatSize,
  logNotice,
  tailRows,
  truncateLog,
  utf8Length,
  type LogStreamHandlers,
  type LogStreamQuery,
  type Timers,
} from "./log-stream";

const MiB = 1024 * 1024;

describe("utf8Length", () => {
  it("counts UTF-8 bytes, not UTF-16 code units", () => {
    expect(utf8Length("abc")).toBe(3);
    expect(utf8Length("é")).toBe(2);
    expect(utf8Length("€")).toBe(3);
    expect(utf8Length("🪲")).toBe(4);
    expect(utf8Length("a€🪲\n")).toBe(new TextEncoder().encode("a€🪲\n").length);
  });
});

describe("backoffDelay", () => {
  it("doubles from 1s up to 30s", () => {
    const top = () => 0; // no jitter taken off
    expect([0, 1, 2, 3, 4, 5, 6, 10].map((n) => backoffDelay(n, top))).toEqual([
      1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000,
    ]);
  });

  it("takes up to a quarter off at random", () => {
    expect(backoffDelay(0, () => 1)).toBe(750);
    expect(backoffDelay(5, () => 1)).toBe(22500);
    expect(backoffDelay(2, () => 0.5)).toBe(3500);
  });
});

describe("appendLog", () => {
  it("keeps chunks as they came, a line split across two included", () => {
    let b = appendLog(emptyLogBuffer(), "one\ntw");
    b = appendLog(b, "o\nthree\n");
    expect(b.text).toBe("one\ntwo\nthree\n");
    expect(b.bytes).toBe(14);
    expect(b.newlines).toBe(3);
  });

  it("past the cap, drops whole lines from the top and counts their bytes", () => {
    let b = appendLog(emptyLogBuffer(), "aaaa\nbbbb\n", 12);
    expect(b.droppedBytes).toBe(0);
    b = appendLog(b, "cccc\n", 12);
    // 15 bytes over a cap of 12: the first line (5 bytes) goes.
    expect(b.text).toBe("bbbb\ncccc\n");
    expect(b.bytes).toBe(10);
    expect(b.droppedBytes).toBe(5);
    expect(b.droppedLines).toBe(1);
    expect(b.newlines).toBe(2);
  });

  it("counts dropped bytes in UTF-8", () => {
    const b = appendLog(appendLog(emptyLogBuffer(), "€€\n", 8), "x\n", 5);
    expect(b.text).toBe("x\n");
    expect(b.droppedBytes).toBe(7);
  });

  it("never cuts the last line, even when it alone is over the cap", () => {
    const b = appendLog(emptyLogBuffer(), "a\nbbbbbbbbbb", 4);
    expect(b.text).toBe("bbbbbbbbbb");
    expect(b.droppedBytes).toBe(2);
  });
});

describe("logNotice", () => {
  it("says nothing when the whole log is shown", () => {
    expect(logNotice(appendLog(emptyLogBuffer(), "line\n"))).toBeNull();
  });

  it("names what shows, the whole size, and where line numbers start when the server sent only the end", () => {
    let b = truncateLog(emptyLogBuffer(), { skippedBytes: 12.3 * MiB, totalBytes: 14.3 * MiB });
    b = appendLog(b, "x".repeat(2 * MiB - 1) + "\n");
    expect(logNotice(b)).toEqual({
      text: "Showing the last 2.0 MB of 14.3 MB. Line numbers start here.",
      loadFull: false,
    });
    // Line numbers stay relative: the first line shown is 1.
    expect(tailRows(b, 1)[0]!.n).toBe(1);
  });

  it("says line numbers start at the first line loaded once the pane dropped some of it too", () => {
    let b = truncateLog(emptyLogBuffer(), { skippedBytes: 100, totalBytes: 110 });
    b = appendLog(b, "aaaa\nbbbb\n", 6);
    expect(logNotice(b)?.text).toBe("Showing the last 5 B of 110 B. Line numbers start at the first line loaded.");
    expect(tailRows(b, 5)).toEqual([{ n: 2, line: "bbbb" }]);
  });

  it("offers Load full log when the whole log fits in 8 MB", () => {
    let b = truncateLog(emptyLogBuffer(), { skippedBytes: 3 * MiB, totalBytes: 5 * MiB });
    b = appendLog(b, "x".repeat(2 * MiB - 1) + "\n");
    expect(logNotice(b)).toEqual({
      text: "Showing the last 2.0 MB of 5.0 MB. Line numbers start here.",
      loadFull: true,
    });
  });

  it("counts lines the pane dropped, whose numbers stay put", () => {
    const b = appendLog(appendLog(emptyLogBuffer(), "a".repeat(1023) + "\n", 2048), "b".repeat(1500) + "\n", 2048);
    expect(logNotice(b)?.text).toBe("Showing the last 1.5 KB of 2.5 KB.");
  });
});

describe("formatSize", () => {
  it("uses powers of 1024 with one decimal", () => {
    expect(formatSize(512)).toBe("512 B");
    expect(formatSize(1536)).toBe("1.5 KB");
    expect(formatSize(2 * MiB)).toBe("2.0 MB");
    expect(formatSize(3 * 1024 * MiB)).toBe("3.0 GB");
  });
});

describe("tailRows", () => {
  it("numbers the last lines from the first line received, past dropped ones", () => {
    let b = appendLog(emptyLogBuffer(), "l1\nl2\n\nl4\nl5\n", 100);
    expect(tailRows(b, 3)).toEqual([
      { n: 4, line: "l4" },
      { n: 5, line: "l5" },
    ]);
    b = appendLog(b, "l6", 10);
    // 15 bytes over a cap of 10: "l1\n" and "l2\n" go.
    expect(b.droppedLines).toBe(2);
    expect(b.text).toBe("\nl4\nl5\nl6");
    expect(tailRows(b, 10)).toEqual([
      { n: 4, line: "l4" },
      { n: 5, line: "l5" },
      { n: 6, line: "l6" },
    ]);
  });
});

/** A fake server: each `open` is one stream the test drives by hand. */
function harness(opts: { outcome: () => string | undefined; tail?: number }) {
  const opened: { q: LogStreamQuery; h: LogStreamHandlers; closed: boolean }[] = [];
  const pending: { fn: () => void; ms: number; cleared: boolean }[] = [];
  const timers: Timers = {
    set: (fn, ms) => {
      const t = { fn, ms, cleared: false };
      pending.push(t);
      return t;
    },
    clear: (t) => {
      (t as { cleared: boolean }).cleared = true;
    },
  };
  const shown: string[] = [];
  let ends = 0;
  let resets = 0;
  const f = followLog({
    open: (q, h) => {
      const s = { q, h, closed: false };
      opened.push(s);
      return () => {
        s.closed = true;
      };
    },
    tail: opts.tail ?? 2 * MiB,
    outcome: opts.outcome,
    onChunk: (t) => shown.push(t),
    onTruncated: () => {},
    onEnd: () => ends++,
    onReset: () => resets++,
    timers,
    random: () => 0,
  });
  /** Run the next timer that is due. */
  const tick = () => {
    const t = pending.shift();
    expect(t).toBeDefined();
    if (!t!.cleared) t!.fn();
    return t!.ms;
  };
  return { f, opened, pending, shown, tick, ends: () => ends, resets: () => resets };
}

describe("followLog", () => {
  it("opens with the tail, and resumes after the last event id", () => {
    const h = harness({ outcome: () => "running" });
    expect(h.opened[0]!.q).toEqual({ tail: 2 * MiB });
    h.opened[0]!.h.onChunk("one\n", "a1:1");
    h.opened[0]!.h.onChunk("two\n", "a1:2");
    h.opened[0]!.h.onEnd!();
    expect(h.opened[0]!.closed).toBe(true);
    expect(h.tick()).toBe(1000);
    expect(h.opened).toHaveLength(2);
    expect(h.opened[1]!.q).toEqual({ from: "a1:2" });
    expect(h.f.lastId()).toBe("a1:2");
  });

  it("backs off 1s, 2s, 4s while reopened streams deliver nothing, and resets once one does", () => {
    const h = harness({ outcome: () => "running" });
    h.opened[0]!.h.onChunk("x\n", "a1:1");
    const waits: number[] = [];
    for (let i = 0; i < 3; i++) {
      h.opened[h.opened.length - 1]!.h.onEnd!();
      waits.push(h.tick());
    }
    expect(waits).toEqual([1000, 2000, 4000]);
    // The fourth stream delivers an event: the next drop waits 1s again.
    const s = h.opened[h.opened.length - 1]!;
    s.h.onChunk("y\n", "a1:2");
    s.h.onEnd!();
    expect(h.tick()).toBe(1000);
    expect(h.opened[h.opened.length - 1]!.q).toEqual({ from: "a1:2" });
  });

  it("after 3 reopens in a row deliver no event, drops the cursor and opens fresh with the tail", () => {
    // A cursor the server refuses (400) ends each reopen with no event.
    const h = harness({ outcome: () => "running" });
    h.opened[0]!.h.onChunk("x\n", "a1:1");
    const last = () => h.opened[h.opened.length - 1]!;
    const waits: number[] = [];
    for (let i = 0; i < 4; i++) {
      last().h.onEnd!();
      waits.push(h.tick());
    }
    // The backoff goes on from where it was.
    expect(waits).toEqual([1000, 2000, 4000, 8000]);
    expect(h.opened.map((s) => s.q)).toEqual([
      { tail: 2 * MiB },
      { from: "a1:1" },
      { from: "a1:1" },
      { from: "a1:1" },
      { tail: 2 * MiB },
    ]);
    expect(h.f.lastId()).toBeNull();
    // The old text stays until the fresh stream delivers; then the pane clears
    // once, before that first event.
    expect(h.resets()).toBe(0);
    last().h.onTruncated!({ skippedBytes: 10, totalBytes: 20 });
    last().h.onChunk("y\n", "a1:4");
    expect(h.resets()).toBe(1);
    // Resuming works again, with the backoff reset.
    last().h.onEnd!();
    expect(h.tick()).toBe(1000);
    expect(last().q).toEqual({ from: "a1:4" });
    expect(h.resets()).toBe(1);
  });

  it("stops when the try has ended", () => {
    let outcome = "running";
    const h = harness({ outcome: () => outcome });
    h.opened[0]!.h.onChunk("x\n", "a1:1");
    h.opened[0]!.h.onEnd!();
    outcome = "succeeded";
    h.tick();
    expect(h.opened).toHaveLength(1);
    expect(h.pending).toHaveLength(0);
    // A stream that ends after the try did is not opened again.
    const h2 = harness({ outcome: () => "failed" });
    h2.opened[0]!.h.onChunk("x\n", "a1:1");
    h2.opened[0]!.h.onEnd!();
    expect(h2.pending).toHaveLength(0);
    expect(h2.opened).toHaveLength(1);
    expect(h2.ends()).toBe(1);
  });

  it("does not reopen while out of view, and decides when back", () => {
    let inView = true;
    const h = harness({ outcome: () => (inView ? "running" : undefined) });
    h.opened[0]!.h.onChunk("x\n", "a1:1");
    h.opened[0]!.h.onEnd!();
    inView = false;
    h.tick();
    expect(h.opened).toHaveLength(1);
    inView = true;
    h.f.inView();
    expect(h.opened).toHaveLength(2);
    expect(h.opened[1]!.q).toEqual({ from: "a1:1" });
  });

  it("close stops the stream and any reopen", () => {
    const h = harness({ outcome: () => "running" });
    h.opened[0]!.h.onEnd!();
    h.f.close();
    h.tick();
    expect(h.opened).toHaveLength(1);
    // Events from a closed stream are ignored.
    h.opened[0]!.h.onChunk("late\n", "a1:9");
    expect(h.shown).toEqual([]);
  });
});
