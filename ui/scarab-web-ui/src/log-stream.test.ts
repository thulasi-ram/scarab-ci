import { describe, it, expect } from "vitest";
import {
  addMarker,
  appendLog,
  backoffDelay,
  emptyLogBuffer,
  followLog,
  formatSize,
  LOG_OPEN_TIMEOUT_MS,
  LOG_QUIET_MS,
  logNotice,
  parseLogMarker,
  tailRows,
  truncateLog,
  utf8Length,
  type LogMarker,
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

  it("does not cut the last line while it alone is under twice the cap", () => {
    const b = appendLog(emptyLogBuffer(), "a\nbbbbbbb", 4);
    expect(b.text).toBe("bbbbbbb");
    expect(b.droppedBytes).toBe(2);
  });

  it("cuts the start of a log with no line breaks once it is over twice the cap", () => {
    const cap = 10;
    let b = emptyLogBuffer();
    for (let i = 0; i < 3; i++) b = appendLog(b, "x".repeat(cap), cap);
    expect(b.bytes).toBeLessThanOrEqual(2 * cap);
    expect(b.text.length).toBe(b.bytes);
    expect(b.droppedBytes).toBeGreaterThan(0);
    expect(b.bytes + b.droppedBytes).toBe(3 * cap);
    expect(logNotice(b)?.text).toBe("Showing the last 10 B of 30 B.");
  });

  it("cuts a line with no breaks on a character boundary", () => {
    // 4-byte characters (two UTF-16 units each) over a cap of 6 bytes.
    const b = appendLog(emptyLogBuffer(), "🪲".repeat(5), 6);
    expect(b.text).toBe("🪲");
    expect(b.bytes).toBe(4);
    expect(b.droppedBytes).toBe(16);
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
      text: "Showing the last 2.0 MiB of 14.3 MiB. Line numbers start here.",
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

  it("offers Load full log when the whole log fits in 8 MiB", () => {
    let b = truncateLog(emptyLogBuffer(), { skippedBytes: 3 * MiB, totalBytes: 5 * MiB });
    b = appendLog(b, "x".repeat(2 * MiB - 1) + "\n");
    expect(logNotice(b)).toEqual({
      text: "Showing the last 2.0 MiB of 5.0 MiB. Line numbers start here.",
      loadFull: true,
    });
  });

  // Regression: with the server's last 2 MiB of a 19,800,019-byte log still
  // arriving, the notice added what had come (1.0 MiB) to what was skipped
  // and said "Showing the last 1.0 MB of 17.9 MB".
  it("names the server's size while the tail is still arriving, and a bigger one as more comes", () => {
    const total = 19_800_019;
    let b = truncateLog(emptyLogBuffer(), { skippedBytes: total - 2 * MiB, totalBytes: total });
    b = appendLog(b, "x".repeat(MiB - 1) + "\n");
    expect(logNotice(b)?.text).toBe("Showing the last 1.0 MiB of 18.9 MiB. Line numbers start here.");
    b = appendLog(b, "y".repeat(MiB - 1) + "\n");
    expect(logNotice(b)?.text).toBe("Showing the last 2.0 MiB of 18.9 MiB. Line numbers start here.");
    // Stored after the stream opened: the whole size grows with it.
    b = appendLog(b, "z".repeat(MiB - 1) + "\n");
    expect(logNotice(b, { outcome: "running" })?.text).toBe(
      "Showing the last 3.0 MiB of 19.9 MiB so far. Line numbers start here.",
    );
  });

  it("says the log is still being stored after its try ended, and offers no Load full log then", () => {
    let b = truncateLog(emptyLogBuffer(), { skippedBytes: 3 * MiB, totalBytes: 5 * MiB });
    b = appendLog(b, "x".repeat(2 * MiB - 1) + "\n");
    expect(logNotice(b, { outcome: "succeeded", store: "storing" })).toEqual({
      text: "Showing the last 2.0 MiB of 5.0 MiB so far. Line numbers start here. The log is still being stored.",
      loadFull: false,
    });
    // Nothing hidden: only that it is still being stored.
    const line = appendLog(emptyLogBuffer(), "line\n");
    expect(logNotice(line, { outcome: "failed", store: "storing" })).toEqual({
      text: "The log is still being stored.",
      loadFull: false,
    });
    // The server stopped waiting for it: reload to check again.
    expect(logNotice(line, { outcome: "failed", store: "wait_over" })?.text).toBe(
      "The log is still being stored; reload to check again.",
    );
    // With no output yet the log body says it, not the notice.
    expect(logNotice(emptyLogBuffer(), { outcome: "failed", store: "storing" })).toBeNull();
  });

  // Regression: a running try's tail holds its lease while it reads, and the
  // notice said "The log is still being stored." for every running step and
  // hid Load full log.
  it("says nothing about storing for a running try, and keeps Load full log", () => {
    let b = truncateLog(emptyLogBuffer(), { skippedBytes: 3 * MiB, totalBytes: 5 * MiB });
    b = appendLog(b, "x".repeat(2 * MiB - 1) + "\n");
    expect(logNotice(b, { outcome: "running", store: "storing" })).toEqual({
      text: "Showing the last 2.0 MiB of 5.0 MiB so far. Line numbers start here.",
      loadFull: true,
    });
    expect(logNotice(appendLog(emptyLogBuffer(), "line\n"), { outcome: "running", store: "storing" })).toBeNull();
  });

  it("counts lines the pane dropped, whose numbers stay put", () => {
    const b = appendLog(appendLog(emptyLogBuffer(), "a".repeat(1023) + "\n", 2048), "b".repeat(1500) + "\n", 2048);
    expect(logNotice(b)?.text).toBe("Showing the last 1.5 KiB of 2.5 KiB.");
  });
});

describe("formatSize", () => {
  it("uses powers of 1024 with one decimal, named KiB, MiB and GiB", () => {
    expect(formatSize(512)).toBe("512 B");
    expect(formatSize(1536)).toBe("1.5 KiB");
    expect(formatSize(2 * MiB)).toBe("2.0 MiB");
    expect(formatSize(3 * 1024 * MiB)).toBe("3.0 GiB");
    // The reported log: 19,800,019 bytes stored.
    expect(formatSize(19_800_019)).toBe("18.9 MiB");
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

describe("parseLogMarker", () => {
  it("reads the server's note: its kind, its reason, and its text", () => {
    expect(parseLogMarker('{"kind":"source_restarted","text":"the log source restarted"}')).toEqual({
      kind: "source_restarted",
      text: "the log source restarted",
    });
    expect(parseLogMarker('{"kind":"not_stored","reason":"ledger_purged","text":"the rest was not stored"}')).toEqual({
      kind: "not_stored",
      reason: "ledger_purged",
      text: "the rest was not stored",
    });
    // A kind this pane does not know still shows its text.
    expect(parseLogMarker('{"kind":"later_kind","text":"a note"}')?.text).toBe("a note");
  });

  it("is null for data that is not a note with a text", () => {
    for (const data of ["", "not json", "null", "[]", '{"kind":"incomplete"}', '{"kind":"incomplete","text":"  "}']) {
      expect(parseLogMarker(data)).toBeNull();
    }
  });
});

describe("addMarker", () => {
  const restarted: LogMarker = { kind: "source_restarted", text: "the log source restarted" };
  const incomplete: LogMarker = { kind: "incomplete", text: "the end of this log could not be read" };

  it("is a row of its own between the lines, with no number of its own", () => {
    let b = appendLog(emptyLogBuffer(), "one\ntwo\n");
    b = addMarker(b, restarted);
    b = appendLog(b, "three\n");
    expect(tailRows(b, 10)).toEqual([
      { n: 1, line: "one" },
      { n: 2, line: "two" },
      { marker: restarted },
      { n: 3, line: "three" },
    ]);
    // The note is not output: the text and its size are the output's.
    expect(b.text).toBe("one\ntwo\nthree\n");
  });

  it("ends the line the output was in the middle of, and the output after it starts a new line", () => {
    let b = appendLog(emptyLogBuffer(), "one\ntw");
    b = addMarker(b, restarted);
    b = appendLog(b, "o\n");
    expect(tailRows(b, 10)).toEqual([
      { n: 1, line: "one" },
      { n: 2, line: "tw" },
      { marker: restarted },
      { n: 3, line: "o" },
    ]);
  });

  it("shows notes at the start, at the end, and with no output at all", () => {
    let b = addMarker(emptyLogBuffer(), restarted);
    expect(tailRows(b, 10)).toEqual([{ marker: restarted }]);
    b = appendLog(b, "one\n");
    b = addMarker(b, incomplete);
    expect(tailRows(b, 10)).toEqual([{ marker: restarted }, { n: 1, line: "one" }, { marker: incomplete }]);
  });

  it("leaves out a note above the last `max` lines, and drops one with the lines above it", () => {
    let b = appendLog(emptyLogBuffer(), "l1\n", 9);
    b = addMarker(b, restarted, 9);
    b = appendLog(b, "l2\nl3\n", 9);
    expect(tailRows(b, 1)).toEqual([{ n: 3, line: "l3" }]);
    expect(tailRows(b, 2)).toEqual([{ marker: restarted }, { n: 2, line: "l2" }, { n: 3, line: "l3" }]);
    // 12 bytes over a cap of 9: "l1\n" goes, and the note after it stays.
    b = appendLog(b, "l4\n", 9);
    expect(b.droppedLines).toBe(1);
    expect(tailRows(b, 10)[0]).toEqual({ marker: restarted });
    // "l2\n" goes too: the note above it goes with it.
    b = appendLog(b, "l5\n", 9);
    expect(b.droppedLines).toBe(2);
    expect(b.markers).toEqual([]);
    expect(tailRows(b, 10)).toEqual([
      { n: 3, line: "l3" },
      { n: 4, line: "l4" },
      { n: 5, line: "l5" },
    ]);
  });

  it("lets the notice say a log with only a note is still being stored", () => {
    const b = addMarker(emptyLogBuffer(), restarted);
    expect(logNotice(b, { outcome: "succeeded", store: "storing" })?.text).toBe("The log is still being stored.");
  });
});

/** A fake server: each `open` is one stream the test drives by hand.
 * `probe` answers the follower's question about a cursor. Timers run only by
 * `tick`, earliest first. */
function harness(opts: {
  outcome: () => string | undefined;
  tail?: number;
  probe?: (q: LogStreamQuery) => Promise<number | null>;
}) {
  const opened: { q: LogStreamQuery; h: LogStreamHandlers; closed: boolean }[] = [];
  type T = { fn: () => void; ms: number; due: number; cleared: boolean };
  const all: T[] = [];
  let clock = 0;
  const timers: Timers = {
    set: (fn, ms) => {
      const t = { fn, ms, due: clock + ms, cleared: false };
      all.push(t);
      return t;
    },
    clear: (t) => {
      (t as T).cleared = true;
    },
    now: () => clock,
  };
  const shown: string[] = [];
  const marks: LogMarker[] = [];
  const errors: (string | null)[] = [];
  let ends = 0;
  let resets = 0;
  let loaded = 0;
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
    onMarker: (m) => marks.push(m),
    onTruncated: () => {},
    onEnd: () => ends++,
    onReset: () => resets++,
    onLoaded: () => loaded++,
    onError: (r) => errors.push(r),
    probe: opts.probe,
    timers,
    random: () => 0,
  });
  /** The timers set and not cleared nor run. */
  const live = () => all.filter((t) => !t.cleared);
  /** Run the timer that is due first, moving the clock to it. A cleared
   * timer runs nothing. */
  const tick = () => {
    const pool = live().length > 0 ? live() : all;
    expect(pool.length).toBeGreaterThan(0);
    const t = pool.reduce((a, b) => (b.due < a.due ? b : a));
    all.splice(all.indexOf(t), 1);
    clock = Math.max(clock, t.due);
    if (!t.cleared) t.fn();
    return t.ms;
  };
  /** Move the clock on. */
  const advance = (ms: number) => {
    clock += ms;
  };
  return {
    f,
    opened,
    get pending() {
      return live();
    },
    shown,
    marks,
    errors,
    tick,
    advance,
    ends: () => ends,
    resets: () => resets,
    loaded: () => loaded,
  };
}

/** Let a probe's answer arrive. */
const settle = () => new Promise((r) => setTimeout(r, 0));

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

  // The server's note has an id like a data event's: a stream that drops
  // right after it resumes after the note, so the note does not show twice.
  it("resumes after a marker's id, and a marker is an answer for the log", () => {
    const h = harness({ outcome: () => "running" });
    const m: LogMarker = { kind: "source_restarted", text: "the log source restarted" };
    h.opened[0]!.h.onChunk("one\n", "a1:1");
    h.opened[0]!.h.onMarker!(m, "a1:2");
    expect(h.marks).toEqual([m]);
    expect(h.f.lastId()).toBe("a1:2");
    h.opened[0]!.h.onEnd!();
    h.tick();
    expect(h.opened[1]!.q).toEqual({ from: "a1:2" });
    const only = harness({ outcome: () => "failed" });
    only.opened[0]!.h.onMarker!(m, "a1:1");
    expect(only.loaded()).toBe(1);
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

  it("after 3 reopens in a row fail before opening and the server refuses the cursor, opens fresh with the tail", async () => {
    // A cursor the server refuses (400) ends each reopen before it opens.
    const probed: LogStreamQuery[] = [];
    const h = harness({
      outcome: () => "running",
      probe: async (q) => {
        probed.push(q);
        return 400;
      },
    });
    h.opened[0]!.h.onChunk("x\n", "a1:1");
    const last = () => h.opened[h.opened.length - 1]!;
    const waits: number[] = [];
    for (let i = 0; i < 4; i++) {
      last().h.onEnd!();
      waits.push(h.tick());
    }
    await settle();
    expect(probed).toEqual([{ from: "a1:1" }]);
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

  it("keeps the cursor when the server does not answer the probe", async () => {
    // The server is down: reopens fail, and so does the probe.
    const h = harness({ outcome: () => "running", probe: async () => null });
    h.opened[0]!.h.onChunk("x\n", "a1:1");
    const last = () => h.opened[h.opened.length - 1]!;
    for (let i = 0; i < 4; i++) {
      last().h.onEnd!();
      h.tick();
    }
    await settle();
    expect(h.opened.slice(1).map((s) => s.q)).toEqual(Array(4).fill({ from: "a1:1" }));
    expect(h.f.lastId()).toBe("a1:1");
    expect(h.resets()).toBe(0);
  });

  it("a quiet stream that stays open longer than the wait before it starts the backoff over", () => {
    const h = harness({ outcome: () => "running" });
    h.opened[0]!.h.onChunk("x\n", "a1:1");
    const last = () => h.opened[h.opened.length - 1]!;
    const waits: number[] = [];
    // Each reopened stream opens, gets only keepalive comments for 5 s, and
    // drops.
    for (let i = 0; i < 6; i++) {
      last().h.onEnd!();
      waits.push(h.tick());
      last().h.onOpen!();
      h.advance(5000);
    }
    expect(waits).toEqual(Array(6).fill(1000));
  });

  it("does not reload a quiet step whose opened streams drop with no event", () => {
    const h = harness({ outcome: () => "running" });
    h.opened[0]!.h.onChunk("x\n", "a1:1");
    const last = () => h.opened[h.opened.length - 1]!;
    // Opened, then only keepalive comments, then dropped: four times over.
    for (let i = 0; i < 4; i++) {
      last().h.onEnd!();
      h.tick();
      last().h.onOpen!();
    }
    expect(h.opened.slice(1).map((s) => s.q)).toEqual(Array(4).fill({ from: "a1:1" }));
    expect(h.f.lastId()).toBe("a1:1");
    expect(h.resets()).toBe(0);
  });

  it("a reopen that opens resets the count of failed ones", () => {
    const h = harness({ outcome: () => "running" });
    h.opened[0]!.h.onChunk("x\n", "a1:1");
    const last = () => h.opened[h.opened.length - 1]!;
    // Two fail, one opens, two fail: never three in a row.
    for (const opens of [false, false, true, false, false]) {
      last().h.onEnd!();
      h.tick();
      if (opens) last().h.onOpen!();
    }
    last().h.onEnd!();
    h.tick();
    expect(h.opened.slice(1).map((s) => s.q)).toEqual(Array(6).fill({ from: "a1:1" }));
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

  // Regression: a stream that ended while the server said the log was still
  // being stored was taken as the whole log of a finished try.
  it("reopens a finished try's stream that ended while its log was still being stored", () => {
    const h = harness({ outcome: () => "succeeded" });
    h.opened[0]!.h.onStoring!(true, false);
    h.opened[0]!.h.onChunk("x\n", "a1:1");
    h.opened[0]!.h.onEnd!();
    h.tick();
    expect(h.opened).toHaveLength(2);
    expect(h.opened[1]!.q).toEqual({ from: "a1:1" });
    // The server says it is whole, then ends: done.
    h.opened[1]!.h.onStoring!(false, false);
    h.opened[1]!.h.onEnd!();
    expect(h.pending).toHaveLength(0);
    expect(h.opened).toHaveLength(2);
  });

  // A tail that hangs keeps its lease: the server stops waiting after 10
  // minutes and says so. The pane does not reopen for another 10 minutes.
  it("does not reopen a stream the server ended because it stopped waiting for the log", () => {
    const h = harness({ outcome: () => "succeeded" });
    h.opened[0]!.h.onStoring!(true, false);
    h.opened[0]!.h.onChunk("x\n", "a1:1");
    h.opened[0]!.h.onStoring!(true, true);
    h.opened[0]!.h.onEnd!();
    expect(h.pending).toHaveLength(0);
    expect(h.opened).toHaveLength(1);
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

// Regression: a finished try's log that was still on its way showed "No log
// output was captured for this try.", and a log that could not be read said
// the same.
describe("followLog: loading and errors", () => {
  it("is loaded on the first data, not on the tail notice before it", () => {
    const h = harness({ outcome: () => "succeeded" });
    h.opened[0]!.h.onOpen!();
    h.opened[0]!.h.onTruncated!({ skippedBytes: 10, totalBytes: 20 });
    expect(h.loaded()).toBe(0);
    h.opened[0]!.h.onChunk("x\n", "a1:1");
    expect(h.loaded()).toBe(1);
    h.opened[0]!.h.onChunk("y\n", "a1:2");
    expect(h.loaded()).toBe(1);
  });

  it("is loaded once an open stream stays quiet, or ends after it opened", () => {
    const h = harness({ outcome: () => "failed" });
    h.opened[0]!.h.onOpen!();
    expect(h.loaded()).toBe(0);
    expect(h.tick()).toBe(LOG_QUIET_MS);
    expect(h.loaded()).toBe(1);
    const h2 = harness({ outcome: () => "failed" });
    h2.opened[0]!.h.onOpen!();
    h2.opened[0]!.h.onEnd!();
    expect(h2.loaded()).toBe(1);
    expect(h2.errors).toEqual([]);
  });

  it("reports the server's error event; a later stream that opens clears it", () => {
    const h = harness({ outcome: () => "running" });
    h.opened[0]!.h.onOpen!();
    h.opened[0]!.h.onError!("log storage read failed: timed out");
    h.opened[0]!.h.onEnd!();
    expect(h.errors).toEqual(["log storage read failed: timed out"]);
    h.tick();
    expect(h.opened).toHaveLength(2);
    h.opened[1]!.h.onOpen!();
    expect(h.errors).toEqual(["log storage read failed: timed out", null]);
  });

  it("a finished try's stream that closes before it opens is an error, with the server's status", async () => {
    const h = harness({ outcome: () => "succeeded", probe: async () => 500 });
    h.opened[0]!.h.onEnd!();
    await settle();
    expect(h.errors).toEqual(["the server answered 500"]);
    expect(h.loaded()).toBe(0);
    const down = harness({ outcome: () => "failed", probe: async () => null });
    down.opened[0]!.h.onEnd!();
    await settle();
    expect(down.errors).toEqual(["the server did not answer"]);
    const noProbe = harness({ outcome: () => "failed" });
    noProbe.opened[0]!.h.onEnd!();
    expect(noProbe.errors).toEqual(["the stream closed before it opened"]);
  });

  it("a running try's streams that keep failing to open are an error, and it keeps trying", () => {
    const h = harness({ outcome: () => "running" });
    for (let i = 0; i < 2; i++) {
      h.opened[h.opened.length - 1]!.h.onEnd!();
      h.tick();
    }
    expect(h.errors).toEqual([]);
    h.opened[h.opened.length - 1]!.h.onEnd!();
    expect(h.errors).toEqual(["the stream closed before it opened, 3 times in a row; trying again"]);
    h.tick();
    expect(h.opened).toHaveLength(4);
    h.opened[3]!.h.onChunk("x\n", "a1:1");
    expect(h.errors.at(-1)).toBeNull();
    expect(h.loaded()).toBe(1);
  });

  it("a stream that does not open in time is an error, cleared when it opens late", () => {
    const h = harness({ outcome: () => "succeeded" });
    expect(h.tick()).toBe(LOG_OPEN_TIMEOUT_MS);
    expect(h.errors).toEqual(["no answer from the server in 20 s"]);
    h.opened[0]!.h.onChunk("x\n", "a1:1");
    expect(h.errors).toEqual(["no answer from the server in 20 s", null]);
    expect(h.shown).toEqual(["x\n"]);
  });

  it("a stream that opened and stays quiet past the timeout is not an error", () => {
    const h = harness({ outcome: () => "succeeded" });
    h.opened[0]!.h.onOpen!();
    h.tick();
    h.advance(LOG_OPEN_TIMEOUT_MS * 3);
    expect(h.pending).toHaveLength(0);
    expect(h.errors).toEqual([]);
  });

  it("close stops the open timeout", () => {
    const h = harness({ outcome: () => "succeeded" });
    h.f.close();
    expect(h.pending).toHaveLength(0);
    expect(h.errors).toEqual([]);
  });
});
