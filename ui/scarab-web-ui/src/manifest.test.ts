// The Manifest tab's pure helpers: tokens, the top-level step scan, the
// result marks and the invoke boxes.
import { describe, expect, it } from "vitest";
import {
  fileLines,
  fileSteps,
  firstFailure,
  invokeBoxes,
  LINE_CHARS,
  markWords,
  SHOWN_LINES,
  stepMark,
  stepRoot,
  tokenizeFile,
  tokenizeLine,
  unmappedSteps,
  type Token,
} from "./manifest";

const text = (ts: Token[]) => ts.map((t) => t.text).join("");
const of = (ts: Token[], cls: Token["cls"]) => ts.filter((t) => t.cls === cls).map((t) => t.text);

describe("tokenizeLine", () => {
  it("keeps markup as plain text", () => {
    const line = `    command: [sh, -c, "echo '<script>alert(1)</script>'"] <b>`;
    const ts = tokenizeLine(line);
    expect(text(ts)).toBe(line);
    expect(of(ts, "string")).toEqual([`"echo '<script>alert(1)</script>'"`]);
    expect(ts.some((t) => t.text.includes("<b>") && t.cls === undefined)).toBe(true);
  });

  it("does not start a comment at a # inside a quoted string", () => {
    const ts = tokenizeLine(`    command: [echo, "a # b", 'c # d'] # the real comment`);
    expect(of(ts, "comment")).toEqual(["# the real comment"]);
    expect(of(ts, "string")).toEqual([`"a # b"`, `'c # d'`]);
  });

  it("does not start a comment at a # with no space before it", () => {
    const ts = tokenizeLine("    image: ghcr.io/x/y@sha256#abc");
    expect(of(ts, "comment")).toEqual([]);
  });

  it("reads an apostrophe inside a plain value as text", () => {
    const ts = tokenizeLine("    title: it's done # note");
    expect(of(ts, "string")).toEqual([]);
    expect(of(ts, "comment")).toEqual(["# note"]);
  });

  it("colours keys, dashes, expressions and invoke references", () => {
    const ts = tokenizeLine(`  - id: deploy`);
    expect(ts).toEqual([
      { text: "  " },
      { text: "-", cls: "punct" },
      { text: " " },
      { text: "id", cls: "key" },
      { text: ":", cls: "punct" },
      { text: " deploy" },
    ]);
    expect(of(tokenizeLine(`    environment: \${{ inputs.env }}`), "expr")).toEqual(["${{ inputs.env }}"]);
    expect(of(tokenizeLine(`    run: "x \${{ a }} y"`), "expr")).toEqual(["${{ a }}"]);
    expect(of(tokenizeLine("    invoke: lib:acme::ci::clone"), "ref")).toEqual(["lib:acme::ci::clone"]);
    expect(of(tokenizeLine("  - { id: g, invoke: plugin:argocd/wait-live }"), "key")).toEqual(["id", "invoke"]);
  });

  it("does not end a comment-free expression at a # inside it", () => {
    expect(of(tokenizeLine(`    if: \${{ a #b }} # c`), "comment")).toEqual(["# c"]);
  });
});

describe("tokenizeFile", () => {
  it("reads a block scalar's content as text, colouring only expressions", () => {
    const lines = fileLines(
      ["    command:", "      - |", "        echo hi # not a comment", "        key: \${{ matrix.os }}", "", "    after: 1"].join(
        "\n",
      ),
    );
    const ts = tokenizeFile(lines);
    expect(of(ts[2]!, "comment")).toEqual([]);
    expect(of(ts[3]!, "key")).toEqual([]);
    expect(of(ts[3]!, "expr")).toEqual(["${{ matrix.os }}"]);
    expect(of(ts[5]!, "key")).toEqual(["after"]);
  });
});

describe("tokenizeFile limits", () => {
  it("tokenizes the first SHOWN_LINES lines and cuts a long line to plain text", () => {
    const long = `name: ${"x".repeat(LINE_CHARS + 10)}`;
    const lines = [long, ...Array.from({ length: SHOWN_LINES + 5 }, () => "a: 1")];
    const ts = tokenizeFile(lines);
    expect(ts).toHaveLength(SHOWN_LINES);
    expect(ts[0]).toEqual([{ text: long.slice(0, LINE_CHARS) }]);
    expect(of(ts[1]!, "key")).toEqual(["a"]);
  });

  it("keeps a block scalar's state across a cut line", () => {
    const ts = tokenizeFile(["run: |", `  ${"x".repeat(LINE_CHARS)}`, "  a: # not a comment", "b: 1"]);
    expect(of(ts[2]!, "comment")).toEqual([]);
    expect(of(ts[3]!, "key")).toEqual(["b"]);
  });
});

// Files under the 1 MiB limit that once froze the tab. Each must be read in
// a time that grows with its size; the bound is far above what that takes.
describe("large files", () => {
  const head = 'name: x\nsteps:\n  - id: a\n    image: alpine\n    command: ["true"]\n';
  const within = (f: () => unknown) => {
    const t = performance.now();
    f();
    return performance.now() - t;
  };
  const read = (source: string) => {
    const lines = fileLines(source);
    tokenizeFile(lines);
    fileSteps(lines);
  };

  it("reads one line of 300,000 expression openers", () => {
    const line = "${{".repeat(300_000);
    expect(within(() => tokenizeLine(`    name: ${line}`))).toBeLessThan(500);
    expect(within(() => read(`name: |\n  ${line}\n${head.slice(8)}`))).toBeLessThan(500);
  });

  it("reads a one-line flow list of quoted strings and references", () => {
    const line = `    command: [${Array.from({ length: 90_000 }, () => "'', lib:a").join(", ")}]`;
    expect(within(() => tokenizeLine(line))).toBeLessThan(500);
    expect(within(() => read(`name: x\nsteps:\n  - id: a\n    image: alpine\n${line}\n`))).toBeLessThan(500);
  });

  it("reads a million blank lines and half a million comment lines", () => {
    expect(within(() => read(head + "\n".repeat(1_000_000)))).toBeLessThan(500);
    expect(within(() => read(head + "#\n".repeat(499_000)))).toBeLessThan(500);
  });

  it("reads a value with a long run of spaces", () => {
    const steps = fileSteps(fileLines(`steps:\n  - id: a\n    invoke: a${" ".repeat(400_000)}b\n`));
    expect(steps[0]!.invoke).toBe(`a${" ".repeat(400_000)}b`);
    expect(within(() => read(`steps:\n  - id: a\n    invoke: a${" ".repeat(400_000)}b\n`))).toBeLessThan(500);
  });
});

describe("fileLines", () => {
  it("drops the final newline only", () => {
    expect(fileLines("a\r\nb\n")).toEqual(["a", "b"]);
    expect(fileLines("a\n\n")).toEqual(["a", ""]);
    expect(fileLines("")).toEqual([]);
  });
});

const CI = `# header
name: ci
steps:
  - id: checkout
    invoke: lib:acme::ci::clone

  - id: test
    matrix:
      os: [linux, macos]
    command:
      - |
        - id: not-a-step
        cargo test
    # a comment about deploy
  - image: alpine
    id: "deploy"
  - { id: gate, invoke: plugin:argocd/wait-live }
on:
  push: {}
`;

describe("fileSteps", () => {
  it("finds every top-level item and its id, invoke and lines", () => {
    expect(fileSteps(fileLines(CI))).toEqual([
      { id: "checkout", line: 3, start: 3, end: 4, invokeLine: 4, invoke: "lib:acme::ci::clone" },
      { id: "test", line: 6, start: 6, end: 12, invokeLine: null, invoke: null },
      { id: "deploy", line: 15, start: 14, end: 15, invokeLine: null, invoke: null },
      { id: "gate", line: 16, start: 16, end: 16, invokeLine: 16, invoke: "plugin:argocd/wait-live" },
    ]);
  });

  it("reads the list at any indent", () => {
    const four = "steps:\n    - id: a\n      image: x\n    - id: b\n";
    expect(fileSteps(fileLines(four)).map((s) => [s.id, s.line])).toEqual([
      ["a", 1],
      ["b", 3],
    ]);
    const none = "steps:\n- id: a\n  image: x\n-\n  id: b\nafter: 1\n";
    expect(fileSteps(fileLines(none)).map((s) => [s.id, s.line, s.end])).toEqual([
      ["a", 1, 2],
      ["b", 4, 4],
    ]);
  });

  it("reads a flow item's id on a later line, and only its own keys", () => {
    const src = [
      "steps:",
      "  - {",
      "      image: y,",
      "      id: b, invoke: 'lib:acme::x' }",
      "  - { image: x, with: { id: inner } }",
      '  - { id: "c, d", with: { id: inner, n: "}" }, # a comment',
      "      invoke: lib:z }",
      "  - id: e",
    ].join("\n");
    expect(fileSteps(fileLines(src))).toEqual([
      { id: "b", line: 3, start: 1, end: 3, invokeLine: 3, invoke: "lib:acme::x" },
      { id: "c, d", line: 5, start: 5, end: 6, invokeLine: 6, invoke: "lib:z" },
      { id: "e", line: 7, start: 7, end: 7, invokeLine: null, invoke: null },
    ]);
  });

  it("gives nothing without a block steps: list", () => {
    expect(fileSteps(fileLines("steps: [{ id: a }]\n"))).toEqual([]);
    expect(fileSteps(fileLines("name: x\n"))).toEqual([]);
  });
});

describe("stepRoot and stepMark", () => {
  it("maps matrix cells and inlined steps to their top-level step", () => {
    expect(stepRoot("test")).toBe("test");
    expect(stepRoot("test[os=linux]")).toBe("test");
    expect(stepRoot("checkout/git")).toBe("checkout");
    expect(stepRoot("ship[svc=a/b]/probe[n=1]")).toBe("ship");
  });

  const steps = [
    { id: "checkout/fetch", status: "succeeded" },
    { id: "checkout/verify", status: "succeeded" },
    { id: "test[os=linux]", status: "succeeded" },
    { id: "test[os=macos]", status: "failed" },
    { id: "build", status: "skipped" },
    { id: "deploy", status: "pending" },
    { id: "environment-approval", status: "skipped" },
  ];

  it("aggregates the Steps a top-level step became", () => {
    expect(stepMark("checkout", steps)).toEqual({ kind: "passed", words: "all 2 steps passed", open: "checkout/fetch" });
    expect(stepMark("test", steps)).toEqual({ kind: "mixed", words: "1 of 2 legs failed", open: "test[os=macos]" });
    expect(stepMark("build", steps)).toEqual({ kind: "skipped", words: "skipped", open: "build" });
    expect(stepMark("deploy", steps)).toEqual({ kind: "notrun", words: "not run yet", open: "deploy" });
    expect(stepMark("gone", steps)).toEqual({ kind: "notrun", words: "not in this Run", open: null });
    expect(
      stepMark("t", [
        { id: "t[a=1]", status: "failed" },
        { id: "t[a=2]", status: "failed" },
      ]).kind,
    ).toBe("failed");
    expect(
      stepMark("t", [
        { id: "t[a=1]", status: "succeeded" },
        { id: "t[a=2]", status: "running" },
      ]),
    ).toEqual({ kind: "running", words: "1 of 2 legs running", open: "t[a=2]" });
  });

  it("reads a step that did not run on a finished Run as not run", () => {
    const legs = [
      { id: "t[a=1]", status: "succeeded" },
      { id: "t[a=2]", status: "pending" },
    ];
    expect(stepMark("t", legs, true)).toEqual({
      kind: "running",
      words: "1 of 2 legs passed, the rest not run yet",
      open: "t[a=1]",
    });
    expect(stepMark("t", legs, false)).toEqual({
      kind: "notrun",
      words: "1 of 2 legs passed, the rest not run",
      open: "t[a=1]",
    });
    // A leg left `running` on a finished Run did not finish.
    expect(stepMark("t", [legs[0]!, { id: "t[a=2]", status: "running" }], false).kind).toBe("notrun");
    expect(stepMark("deploy", steps, false)).toEqual({ kind: "notrun", words: "not run", open: "deploy" });
    expect(markWords("pending", true)).toBe("not run yet");
    expect(markWords("not_run", true)).toBe("not run");
    expect(markWords("failed", false)).toBe("failed");
  });

  it("names the steps no line maps to, and the first failure in file order", () => {
    const file = fileSteps(fileLines("steps:\n  - id: checkout\n  - id: test\n  - id: build\n"));
    expect(unmappedSteps(file, steps).map((s) => s.id)).toEqual(["deploy", "environment-approval"]);
    expect(firstFailure(file, steps)?.id).toBe("test");
  });
});

describe("invokeBoxes", () => {
  const step = { id: "build", line: 0, start: 0, end: 1, invokeLine: 1, invoke: "lib:acme::ci::build" };
  const invoked = [
    { kind: "library", name: "acme::ci::clone", content_hash: "c1", steps: ["checkout/git"] },
    { kind: "library", name: "acme::ci::buildkit", content_hash: "k1", steps: ["build/push/kit"] },
    { kind: "library", name: "acme::ci::build", content_hash: "b1", steps: ["build/push/kit", "build/tag"] },
  ];

  it("puts the library the line names first and marks a yanked version", () => {
    const boxes = invokeBoxes(step, invoked, [{ name: "acme::ci::build", content_hash: "b1" }]);
    expect(boxes.map((b) => [b.entry.name, b.direct, b.yanked, b.steps])).toEqual([
      ["acme::ci::build", true, true, ["build/push/kit", "build/tag"]],
      ["acme::ci::buildkit", false, false, ["build/push/kit"]],
    ]);
  });

  it("does not call another version of the same library yanked", () => {
    expect(invokeBoxes(step, invoked, [{ name: "acme::ci::build", content_hash: "old" }])[0]!.yanked).toBe(false);
  });
});
