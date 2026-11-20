// The run page's log pane, without the DOM: how much of a log the pane asks
// for, how much it keeps, the notice when part of a log is not shown, and when
// a stream that ended is opened again.
//
// Sizes are UTF-8 bytes throughout, the unit the server counts in (`tail`,
// `skipped_bytes`, `total_bytes`), so the pane's own cap and the server's
// cutoff add up in one notice.

import { streamEndStep, type StreamEnd } from "./logs";

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

/** Reopens in a row that fail before the stream opens, before the pane asks
 * the server whether it refuses the cursor. A cursor the server refuses (400)
 * fails every reopen that way, and an EventSource reports it only as an error,
 * so it would retry forever. A quiet step's stream opens and gets only
 * keepalive comments, so its drops do not count. */
export const FAILED_REOPENS_BEFORE_FRESH = 3;

/** How long a log stream may take to open before the pane says it could not
 * load the log. The server reads the start of the stored log before it
 * answers, so a store read that hangs, or a proxy that holds the request,
 * shows as a stream that does not open. Silence after the stream opened is
 * not a failure: keepalive comments do not reach the page, and a finished
 * try's stream can stay open and quiet while its run goes on. */
export const LOG_OPEN_TIMEOUT_MS = 20_000;

/** How long an open stream may stay quiet before the pane takes the log as
 * empty for now. The first `data` event can arrive a moment after the stream
 * opens; without this wait a finished try would flash "no output" first. */
export const LOG_QUIET_MS = 1000;

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
  /** The server's `storing` event: the log is still being stored after its
   * try ended (`true`), or no longer is (`false`). `waitOver`: the server
   * stops waiting for it and ends the stream with the log still being
   * stored. */
  onStoring?: (storing: boolean, waitOver: boolean) => void;
  /** The connection opened: the server accepted the request. */
  onOpen?: () => void;
  /** The server's `error` event: reading the stored log failed. The stream
   * ends right after it. */
  onError?: (message: string) => void;
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

/** A size for the notice: `512 B`, `1.5 KiB`, `2.0 MiB` (powers of 1024,
 * named as such: a 19,800,019-byte log is 18.9 MiB, or 19.8 MB). */
export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < MiB) return `${(bytes / 1024).toFixed(1)} KiB`;
  if (bytes < 1024 * MiB) return `${(bytes / MiB).toFixed(1)} MiB`;
  return `${(bytes / (1024 * MiB)).toFixed(1)} GiB`;
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
  /** The log's stored length when the server cut it (`tail`), else 0. */
  totalBytes: number;
  /** Bytes the pane received and dropped to stay under its cap. */
  droppedBytes: number;
  /** Lines the pane dropped, so line numbers stay put after a drop. */
  droppedLines: number;
};

export function emptyLogBuffer(): LogBuffer {
  return { text: "", bytes: 0, newlines: 0, skippedBytes: 0, totalBytes: 0, droppedBytes: 0, droppedLines: 0 };
}

/** Record the server's tail cutoff. */
export function truncateLog(buf: LogBuffer, t: LogTruncation): LogBuffer {
  return { ...buf, skippedBytes: t.skippedBytes, totalBytes: t.totalBytes };
}

/** The start of `s` that is at least `bytes` UTF-8 bytes long, cut on a
 * character boundary: its length in UTF-16 code units and in bytes. */
function headOf(s: string, bytes: number): { units: number; bytes: number } {
  let i = 0;
  let n = 0;
  while (i < s.length && n < bytes) {
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
    i++;
  }
  return { units: i, bytes: n };
}

/** Append a chunk as it came (a chunk is raw output, so it may end mid-line).
 * Past `cap` bytes, whole lines are dropped from the top until the text fits.
 * The last line is not cut while it is under twice the cap; past that, its
 * start is dropped down to `cap` bytes, on a character boundary. */
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
    // A log with no line breaks: what is left is one line. Cut its start.
    if (bytes > 2 * cap) {
      const head = headOf(text, bytes - cap);
      text = text.slice(head.units);
      bytes -= head.bytes;
      droppedBytes += head.bytes;
    }
  }
  return { ...buf, text, bytes, newlines, droppedBytes, droppedLines };
}

/** What the pane says while a try's log is still being stored after the try
 * ended, and once the server stopped waiting for it. */
export const STORING_TEXT = "The log is still being stored.";
export const STORING_WAIT_OVER_TEXT = "The log is still being stored; reload to check again.";

/** Where the server's `storing` events left a log: still being stored, or
 * the server stopped waiting for it with the log still being stored. */
export type StoreState = "storing" | "wait_over";

/** The notice above a log when part of it is not shown, or while it is still
 * being stored after its try ended; else null. `loadFull`: the whole log fits
 * under `FULL_LOG_BYTES`, so the pane can ask for all of it.
 *
 * The whole size is the server's stored length when it cut the log, or more
 * once more has arrived: while the server's tail is still arriving, what came
 * so far plus what was skipped is less than the log. While the try runs, or
 * its log is still being stored, the size is named as the size so far.
 * `store` counts only for a try that is not running: a running try's log is
 * always still being stored, which the notice does not need to say. With no
 * output yet the log body says it instead, so the notice does not.
 *
 * Line numbers count from the first line the pane received. After the
 * server's cutoff that is not line 1 of the log, so the notice says where the
 * numbers start. Lines the pane dropped itself keep their numbers. */
export function logNotice(
  buf: LogBuffer,
  opts: { outcome?: string; store?: StoreState } = {},
): { text: string; loadFull: boolean } | null {
  const running = opts.outcome === "running";
  const store = !running && buf.bytes > 0 ? opts.store : undefined;
  const stored = store === "wait_over" ? STORING_WAIT_OVER_TEXT : store === "storing" ? STORING_TEXT : "";
  const hidden = buf.skippedBytes + buf.droppedBytes;
  if (hidden === 0) return stored ? { text: stored, loadFull: false } : null;
  const total = Math.max(buf.totalBytes, hidden + buf.bytes);
  const sofar = running || store ? " so far" : "";
  let text = `Showing the last ${formatSize(buf.bytes)} of ${formatSize(total)}${sofar}.`;
  if (buf.skippedBytes > 0) {
    text += buf.droppedLines > 0 ? " Line numbers start at the first line loaded." : " Line numbers start here.";
  }
  if (stored) text += ` ${stored}`;
  return { text, loadFull: !store && total <= FULL_LOG_BYTES };
}

/** The last `max` lines of the buffer, numbered from the log's first line the
 * pane received. Empty lines keep their number but are not rows. Splits only
 * the tail, not the whole buffer. */
export function tailRows(buf: LogBuffer, max: number): { n: number; line: string }[] {
  const text = buf.text;
  let start = 0;
  // A final newline ends the last line; it does not start a row of its own.
  let pos = text.endsWith("\n") ? text.length - 1 : text.length;
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
  /** Milliseconds since some fixed point. */
  now: () => number;
};

const realTimers: Timers = {
  set: (fn, ms) => setTimeout(fn, ms),
  clear: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
  now: () => Date.now(),
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
 * reopened stream delivers an event, or stayed open longer than the wait
 * before it. After `FAILED_REOPENS_BEFORE_FRESH` reopens in a row that end
 * before they open, `probe` asks the server about the cursor. Only when it
 * answers 400 is the cursor dropped and the next stream opened with `tail`
 * again; `onReset` clears the pane when that stream delivers its first event.
 * When the server is unreachable or answers anything else, the cursor is
 * kept. The backoff goes on from where it was. `streamEndStep` decides each end: a try
 * that finished is done, and one that finished with no output is read once
 * more. A stream that ends while its last `storing` event said the log is
 * still being stored is reopened like a running try's, unless that event
 * said the server stopped waiting for it. `outcome()` is undefined while the log is out of view; the decision
 * then waits for `inView()`, and the pane stops reopening a try it switched
 * away from. */
export function followLog(opts: {
  open: (q: LogStreamQuery, h: LogStreamHandlers) => () => void;
  tail?: number;
  /** The try's outcome now, or undefined while its log is not in view. */
  outcome: () => string | undefined;
  onChunk: (text: string) => void;
  onTruncated: (t: LogTruncation) => void;
  /** The server's `storing` event (see `LogStreamHandlers`). */
  onStoring?: (storing: boolean, waitOver: boolean) => void;
  /** A stream ended. */
  onEnd: () => void;
  /** A stream opened again. */
  onReopen?: () => void;
  /** A stream opened fresh with `tail` after resuming failed delivered its
   * first event: clear what the pane holds before it arrives. */
  onReset?: () => void;
  /** Ask the server about the stream `q` names without reading it: the HTTP
   * status, or null when the server did not answer. Without it the cursor is
   * always kept. */
  probe?: (q: LogStreamQuery) => Promise<number | null>;
  /** The server answered for this log: a stream delivered an event, ended
   * after it opened, or stayed open and quiet for `LOG_QUIET_MS`. Until then
   * the pane says it is loading the log. Called once. */
  onLoaded?: () => void;
  /** The log could not be read, and why; `null` once a later stream opens or
   * delivers an event. Reported when the server sent an `error` event, when
   * a finished try's stream closed before it opened, when a running try's
   * stream failed to open `FAILED_REOPENS_BEFORE_FRESH` times in a row (it
   * goes on trying), and when a stream did not open within
   * `LOG_OPEN_TIMEOUT_MS`. */
  onError?: (reason: string | null) => void;
  timers?: Timers;
  random?: () => number;
}): LogFollower {
  const timers = opts.timers ?? realTimers;
  let closeStream: (() => void) | null = null;
  let timer: unknown = null;
  // The open timeout and the quiet wait of the stream in flight.
  let openTimer: unknown = null;
  let quietTimer: unknown = null;
  let loaded = false;
  let error: string | null = null;
  // Streams in a row that ended before they opened.
  let failedOpens = 0;
  // Bumped by each stream, so a late probe answer about an older one is
  // dropped.
  let streamSeq = 0;
  let stopped = false;
  let deferred = false;
  let lastId: string | null = null;
  let hadOutput = false;
  let failures = 0;
  // Reopens in a row that ended before they opened.
  let failedReopens = 0;
  // The stream in flight is a fresh open after resuming failed.
  let resetPending = false;
  // The wait before the stream in flight opened.
  let lastWait = 0;
  // The stream in flight said the log is still being stored, and that it
  // stopped waiting for it.
  let storing = false;
  let waitOver = false;

  const clearTimer = (t: unknown) => {
    if (t !== null) timers.clear(t);
    return null;
  };
  const markLoaded = () => {
    quietTimer = clearTimer(quietTimer);
    if (loaded) return;
    loaded = true;
    opts.onLoaded?.();
  };
  const report = (reason: string | null) => {
    if (stopped || error === reason) return;
    error = reason;
    opts.onError?.(reason);
  };

  const start = (reopen: boolean) => {
    if (stopped) return;
    if (reopen && lastId != null && failedReopens >= FAILED_REOPENS_BEFORE_FRESH) {
      // Reopens keep failing: a refused cursor (400) or a server that is
      // down. Ask which before giving up the cursor.
      failedReopens = 0;
      const cursor = lastId;
      const probe = opts.probe ?? (() => Promise.resolve(null));
      const then = (status: number | null) => {
        if (stopped) return;
        if (status === 400 && lastId === cursor) {
          lastId = null;
          resetPending = true;
        }
        openStream(reopen);
      };
      probe({ from: cursor }).then(then, () => then(null));
      return;
    }
    openStream(reopen);
  };

  // A finished try's stream closed before it opened: ask the server why, if
  // it can be asked, and say so unless another stream started meanwhile.
  const reportClosed = (q: LogStreamQuery, seq: number) => {
    const say = (status: number | null) => {
      if (seq !== streamSeq) return;
      report(
        status === null
          ? "the server did not answer"
          : status >= 400
            ? `the server answered ${status}`
            : "the stream closed before it opened",
      );
    };
    if (!opts.probe) return say(200);
    opts.probe(q).then(say, () => say(null));
  };

  const openStream = (reopen: boolean) => {
    if (stopped) return;
    const seq = ++streamSeq;
    // One stream at a time: `current` tells this stream's events from a
    // closed one's.
    const current = {};
    let active: object | null = current;
    let close: (() => void) | null = null;
    const mine = () => active === current && !stopped;
    let opened = false;
    let openedAt = 0;
    const gotOpen = () => {
      if (!opened) openedAt = timers.now();
      opened = true;
      failedReopens = 0;
      failedOpens = 0;
      openTimer = clearTimer(openTimer);
      report(null);
      if (!loaded && quietTimer === null) {
        quietTimer = timers.set(() => {
          quietTimer = null;
          markLoaded();
        }, LOG_QUIET_MS);
      }
    };
    // Any event of this stream: it opened, the backoff starts over, and a
    // fresh open after resuming failed clears the pane first.
    const gotEvent = () => {
      gotOpen();
      failures = 0;
      if (resetPending) {
        resetPending = false;
        hadOutput = false;
        opts.onReset?.();
      }
    };
    const q: LogStreamQuery = lastId != null ? { from: lastId } : { tail: opts.tail };
    // Each stream says it again if the log is still being stored.
    storing = false;
    waitOver = false;
    openTimer = clearTimer(openTimer);
    quietTimer = clearTimer(quietTimer);
    if (reopen) opts.onReopen?.();
    close = opts.open(q, {
      onChunk: (text, id) => {
        if (!mine()) return;
        gotEvent();
        if (id) lastId = id;
        if (text.length > 0) hadOutput = true;
        opts.onChunk(text);
        markLoaded();
      },
      onTruncated: (t) => {
        if (!mine()) return;
        // The tail's first `data` event comes right after this one: the log
        // is not loaded yet.
        gotEvent();
        opts.onTruncated(t);
      },
      onStoring: (s, w) => {
        if (!mine()) return;
        gotEvent();
        storing = s;
        waitOver = w;
        opts.onStoring?.(s, w);
        markLoaded();
      },
      onOpen: () => {
        if (!mine()) return;
        gotOpen();
      },
      onError: (message) => {
        if (!mine()) return;
        report(message);
      },
      onEnd: () => {
        if (!mine()) return;
        openTimer = clearTimer(openTimer);
        if (!opened) failedOpens++;
        else markLoaded();
        if (reopen && !opened) failedReopens++;
        // A quiet stream that stayed open longer than the wait before it was
        // not a failure: the backoff starts over.
        if (opened && timers.now() - openedAt > lastWait) failures = 0;
        active = null;
        closeStream = null;
        close?.();
        opts.onEnd();
        const step = decide(false);
        if (opened) return;
        if (step === "done") reportClosed(q, seq);
        else if (failedOpens === FAILED_REOPENS_BEFORE_FRESH) {
          report(`the stream closed before it opened, ${failedOpens} times in a row; trying again`);
        }
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
    if (!opened) {
      openTimer = timers.set(() => {
        openTimer = null;
        if (mine() && !opened) report(`no answer from the server in ${LOG_OPEN_TIMEOUT_MS / 1000} s`);
      }, LOG_OPEN_TIMEOUT_MS);
    }
  };

  const decide = (graceOver: boolean): StreamEnd | undefined => {
    if (stopped) return undefined;
    const outcome = opts.outcome();
    if (outcome === undefined) {
      deferred = true;
      return undefined;
    }
    // The server says `storing: false` before it ends a stream whose log is
    // whole: a stream that ended while the log was still being stored
    // dropped. Follow it like a running try. When the server said it stopped
    // waiting (a tail that hangs), do not: the pane says to reload.
    const follow = storing && !waitOver;
    const step = streamEndStep({ outcome: follow ? "running" : outcome, graceOver, hadOutput });
    if (step === "reopen") {
      start(true);
    } else if (step === "wait" && timer === null) {
      const wait = backoffDelay(failures++, opts.random);
      timer = timers.set(() => {
        timer = null;
        lastWait = wait;
        decide(true);
      }, wait);
    }
    return step;
  };

  start(false);
  return {
    close: () => {
      stopped = true;
      timer = clearTimer(timer);
      openTimer = clearTimer(openTimer);
      quietTimer = clearTimer(quietTimer);
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
