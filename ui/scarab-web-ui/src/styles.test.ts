// The web UI has four text styles (Title, Label, Body, Mono; docs/DESIGN.md §3),
// so no page uses more than four font variations. This reads the stylesheet
// and fails on any text rule that sets a font outside those four tokens, and
// on upper-casing, which reads as a fifth style.
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const CSS = readFileSync(new URL("./styles.css", import.meta.url), "utf8");

/** The selectors that keep their own face or size (listed in styles.css). */
const EXEMPT = new Set([
  ".brand", // the top-bar wordmark
  ".login-title", // the login lockup under the emblem
  ".ascii-scene pre", // the ASCII scenes size their dots from --ascii-fs
  ".ascii-scene .ascii-fe",
]);

const TOKENS: Record<string, string> = {
  "--t-title": "600 20px/1.3 var(--font-ui)",
  "--t-label": "600 13px/1.5 var(--font-ui)",
  "--t-body": "400 13px/1.5 var(--font-ui)",
  "--t-mono": "400 12px/1.5 var(--font-mono)",
};
const FONT_VALUES = new Set([...Object.keys(TOKENS).map((t) => `var(${t})`), "inherit"]);

type Decl = { selector: string; prop: string; value: string };

/** Every declaration with the selector of the rule it sits in. */
function declarations(css: string): Decl[] {
  const text = css.replace(/\/\*[\s\S]*?\*\//g, "");
  const out: Decl[] = [];
  const stack: string[] = [];
  let buf = "";
  const flush = () => {
    const d = buf.trim();
    buf = "";
    const i = d.indexOf(":");
    if (i < 0 || stack.length === 0) return;
    out.push({
      selector: stack[stack.length - 1],
      prop: d.slice(0, i).trim(),
      value: d.slice(i + 1).replace(/\s+/g, " ").trim(),
    });
  };
  for (const ch of text) {
    if (ch === "{") {
      stack.push(buf.replace(/\s+/g, " ").trim());
      buf = "";
    } else if (ch === "}") {
      flush();
      stack.pop();
    } else if (ch === ";") {
      flush();
    } else {
      buf += ch;
    }
  }
  return out;
}

const DECLS = declarations(CSS);

describe("the four text styles", () => {
  it("reads the stylesheet", () => {
    // A failed read must not pass the checks below with nothing to check.
    expect(DECLS.length).toBeGreaterThan(1000);
  });

  it("defines exactly the four style tokens", () => {
    const defined = DECLS.filter((d) => d.prop.startsWith("--t-"));
    expect(Object.fromEntries(defined.map((d) => [d.prop, d.value]))).toEqual(TOKENS);
  });

  it("sets a font only through one of the four tokens", () => {
    const bad = DECLS.filter((d) => {
      if (EXEMPT.has(d.selector)) return false;
      if (d.prop === "font") return !FONT_VALUES.has(d.value);
      if (d.prop === "font-style") return d.value !== "normal";
      return ["font-size", "font-weight", "font-family"].includes(d.prop);
    }).map((d) => `${d.selector} { ${d.prop}: ${d.value} }`);
    expect(bad).toEqual([]);
  });

  it("never upper-cases text", () => {
    const bad = DECLS.filter((d) => d.prop === "text-transform" && d.value === "uppercase").map(
      (d) => d.selector,
    );
    expect(bad).toEqual([]);
  });
});
