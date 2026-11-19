// The run page's log pane, without the DOM: how much of a log the pane asks
// for, how much it keeps, the notice when part of a log is not shown, and when
// a stream that ended is opened again.
//
// Sizes are UTF-8 bytes throughout, the unit the server counts in (`tail`,
// `skipped_bytes`, `total_bytes`), so the pane's own cap and the server's
// cutoff add up in one notice.

import { streamEndStep } from "./logs";

const MiB = 1024 * 1024;

/** What the log pane asks the server for when it opens a log. */
export const DEFAULT_TAIL_BYTES = 2 * MiB;
/** What Load full log asks for; a log larger than this is downloaded instead. */
export const FULL_LOG_BYTES = 8 * MiB;
/** The most text the pane keeps for one log. */
export const CLIENT_CAP_BYTES = 8 * MiB;

/** The first wait before a dropped stream is opened again, and the longest. */
export const BACKOFF_INITIAL_MS = 1000;
export const BACKOFF_MAX_MS = 30_000;

/** The server's `truncated` event: the replay skipped the first `skippedBytes`
 * of a log that was `totalBytes` long when the stream opened. */
export type LogTruncation = { skippedBytes: number; totalBytes: number };

/** Where a log stream starts: after the event `from` names (a cursor the
 * server sent as an event id), or else the last `tail` bytes. */
export type LogStreamQuery = { from?: string; tail?: number };

export type LogStreamHandlers = {
  /** A `data` event: the text, and its id (the position after it). */
  onChunk: (text: string, id: string | null) => void;
  onTruncated?: (t: LogTruncation) => void;
  /** The stream closed: the server ended it, it failed, or it dropped. */
  onEnd?: () => void;
};

/** The byte length of `s` in UTF-8. A lone surrogate counts as 3 bytes, the
 * size of the replacement character the encoder writes for it. */
export function utf8Length(s: string): number {
  let n = 0;
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c < 0x80) n += 1;
    else if (c < 0x800) n += 2;
    else if (c >= 0xd800 && c <= 0xdbff && i + 1 < s.length) {
      const d = s.charCodeAt(i + 1);
      if (d >= 0xdc00 && d <= 0xdfff) {
        n += 4;
        i++;
      } else n += 3;
    } else n += 3;
  }
  return n;
}

function countNewlines(s: string): number {
  let n = 0;
  for (let i = s.indexOf("\n"); i >= 0; i = s.indexOf("\n", i + 1)) n++;
  return n;
}

/** A size for the notice: `512 B`, `1.5 KB`, `2.0 MB` (powers of 1024). */
export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < MiB) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * MiB) return `${(bytes / MiB).toFixed(1)} MB`;
  return `${(bytes / (1024 * MiB)).toFixed(1)} GB`;
}

/** One log as the pane holds it. */
export type LogBuffer = {
  /** The text on screen. */
  text: string;
  /** `text`'s size in UTF-8 bytes. */
  bytes: number;
  /** The line breaks in `text`. */
  newlines: number;
  /** Bytes at the start of the log the server did not send (`tail`). */
  skippedBytes: number;
  /** Bytes the pane received and dropped to stay under its cap. */
  droppedBytes: number;
  /** Lines the pane dropped, so line numbers stay put after a drop. */
  droppedLines: number;
};

export function emptyLogBuffer(): LogBuffer {
  return { text: "", bytes: 0, newlines: 0, skippedBytes: 0, droppedBytes: 0, droppedLines: 0 };
}

/** Record the server's tail cutoff. */
export function truncateLog(buf: LogBuffer, t: LogTruncation): LogBuffer {
  return { ...buf, skippedBytes: t.skippedBytes };
}

/** Append a chunk as it came (a chunk is raw output, so it may end mid-line).
 * Past `cap` bytes, whole lines are dropped from the top until the text fits.
 * The last line is never cut: when it alone is over the cap, it stays. */
export function appendLog(buf: LogBuffer, chunk: string, cap = CLIENT_CAP_BYTES): LogBuffer {
  if (chunk.length === 0) return buf;
  let text = buf.text + chunk;
  let bytes = buf.bytes + utf8Length(chunk);
  let newlines = buf.newlines + countNewlines(chunk);
  let droppedBytes = buf.droppedBytes;
  let droppedLines = buf.droppedLines;
  if (bytes > cap) {
    const excess = bytes - cap;
    let cut = 0;
    let cutBytes = 0;
    let cutLines = 0;
    while (cutBytes < excess) {
      const nl = text.indexOf("\n", cut);
      if (nl < 0) break;
      cutBytes += utf8Length(text.slice(cut, nl + 1));
      cut = nl + 1;
      cutLines++;
    }
    text = text.slice(cut);
    bytes -= cutBytes;
    newlines -= cutLines;
    droppedBytes += cutBytes;
    droppedLines += cutLines;
  }
  return { ...buf, text, bytes, newlines, droppedBytes, droppedLines };
}

/** The notice above a log when part of it is not shown, or null. `loadFull`:
 * the whole log fits under `FULL_LOG_BYTES`, so the pane can ask for all of it. */
export function logNotice(buf: LogBuffer): { text: string; loadFull: boolean } | null {
  const hidden = buf.skippedBytes + buf.droppedBytes;
  if (hidden === 0) return null;
  const total = hidden + buf.bytes;
  return {
    text: `Showing the last ${formatSize(buf.bytes)} of ${formatSize(total)}.`,
    loadFull: total <= FULL_LOG_BYTES,
  };
}

/** The last `max` lines of the buffer, numbered from the log's first line the
 * pane received. Empty lines keep their number but are not rows. Splits only
 * the tail, not the whole buffer. */
export function tailRows(buf: LogBuffer, max: number): { n: number; line: string }[] {
  const text = buf.text;
  let start = 0;
  let pos = text.length;
  for (let seen = 0; ; ) {
    const p = pos > 0 ? text.lastIndexOf("\n", pos - 1) : -1;
    if (p < 0) break;
    seen++;
    if (seen === max) {
      start = p + 1;
      break;
    }
    pos = p;
  }
  const lines = text.slice(start).split("\n");
  const before = buf.droppedLines + buf.newlines - (lines.length - 1);
  return lines.map((line, i) => ({ n: before + i + 1, line })).filter((r) => r.line.length > 0);
}

/** The wait before the `failures`+1-th reopen in a row: 1s, 2s, 4s … at most
 * 30s, less up to a quarter at random so many panes do not reopen together. */
export function backoffDelay(failures: number, random: () => number = Math.random): number {
  const base = Math.min(BACKOFF_MAX_MS, BACKOFF_INITIAL_MS * 2 ** Math.max(0, failures));
  return Math.round(base * (1 - 0.25 * random()));
}

export type Timers = {
  set: (fn: () => void, ms: number) => unknown;
  clear: (handle: unknown) => void;
};

const realTimers: Timers = {
  set: (fn, ms) => setTimeout(fn, ms),
  clear: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
};

export type LogFollower = {
  /** Stop: close the stream and never open it again. */
  close: () => void;
  /** The log is in view again: decide a reopen that waited for that. */
  inView: () => void;
  /** The id of the last event received, the resume cursor. */
  lastId: () => string | null;
};

/** Follow one try's log across dropped connections.
 *
 * The first stream opens with `tail`. When a stream ends while the try still
 * reads as running, it opens again after a backoff (`backoffDelay`), resuming
 * after the last event id so nothing is sent twice; the backoff resets once a
 * reopened stream delivers an event. `streamEndStep` decides each end: a try
 * that finished is done, and one that finished with no output is read once
 * more. `outcome()` is undefined while the log is out of view; the decision
 * then waits for `inView()`, and the pane stops reopening a try it switched
 * away from. */
export function followLog(opts: {
  open: (q: LogStreamQuery, h: LogStreamHandlers) => () => void;
  tail?: number;
  /** The try's outcome now, or undefined while its log is not in view. */
  outcome: () => string | undefined;
  onChunk: (text: string) => void;
  onTruncated: (t: LogTruncation) => void;
  /** A stream ended. */
  onEnd: () => void;
  /** A stream opened again. */
  onReopen?: () => void;
  timers?: Timers;
  random?: () => number;
}): LogFollower {
  const timers = opts.timers ?? realTimers;
  let closeStream: (() => void) | null = null;
  let timer: unknown = null;
  let stopped = false;
  let deferred = false;
  let lastId: string | null = null;
  let hadOutput = false;
  let failures = 0;

  const start = (reopen: boolean) => {
    if (stopped) return;
    // One stream at a time: `current` tells this stream's events from a
    // closed one's.
    const current = {};
    let active: object | null = current;
    let close: (() => void) | null = null;
    const mine = () => active === current && !stopped;
    const q: LogStreamQuery = lastId != null ? { from: lastId } : { tail: opts.tail };
    if (reopen) opts.onReopen?.();
    close = opts.open(q, {
      onChunk: (text, id) => {
        if (!mine()) return;
        failures = 0;
        if (id) lastId = id;
        if (text.length > 0) hadOutput = true;
        opts.onChunk(text);
      },
      onTruncated: (t) => {
        if (!mine()) return;
        failures = 0;
        opts.onTruncated(t);
      },
      onEnd: () => {
        if (!mine()) return;
        active = null;
        closeStream = null;
        close?.();
        opts.onEnd();
        decide(false);
      },
    });
    // The stream ended before `open` returned.
    if (active !== current) {
      close();
      return;
    }
    closeStream = () => {
      active = null;
      close?.();
    };
  };

  const decide = (graceOver: boolean) => {
    if (stopped) return;
    const outcome = opts.outcome();
    if (outcome === undefined) {
      deferred = true;
      return;
    }
    const step = streamEndStep({ outcome, graceOver, hadOutput });
    if (step === "reopen") {
      start(true);
    } else if (step === "wait" && timer === null) {
      timer = timers.set(() => {
        timer = null;
        decide(true);
      }, backoffDelay(failures++, opts.random));
    }
  };

  start(false);
  return {
    close: () => {
      stopped = true;
      if (timer !== null) timers.clear(timer);
      timer = null;
      closeStream?.();
      closeStream = null;
    },
    inView: () => {
      if (!deferred || stopped) return;
      deferred = false;
      decide(true);
    },
    lastId: () => lastId,
  };
}
