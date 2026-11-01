// Settings → Event log: how the URL becomes a request and the form a URL.
import { describe, expect, it } from "vitest";
import {
  emptyText,
  eventsQuery,
  filterSearch,
  fromLocalInput,
  isFiltered,
  parseInstant,
  rangeProblem,
  readView,
  toLocalInput,
} from "./event-log";

const none = { kind: "", actor: "", target: "", from: "", to: "" };

describe("readView", () => {
  it("reads every param, trimmed, taking the first of a repeated one", () => {
    expect(
      readView({
        kind: "step_up.refused",
        actor: [" j.lee ", "a.kim"],
        target: " orders-api ",
        from: "2026-11-01T09:30Z",
        to: undefined,
        cursor: "c2VxOjQy",
      }),
    ).toEqual({
      kind: "step_up.refused",
      actor: "j.lee",
      target: "orders-api",
      from: "2026-11-01T09:30Z",
      to: "",
      cursor: "c2VxOjQy",
    });
  });
});

describe("eventsQuery", () => {
  it("sends only what is set, with times as unix ms", () => {
    expect(eventsQuery({ ...none, cursor: "" })).toEqual({});
    expect(
      eventsQuery({
        kind: "op.completed",
        actor: "a.kim",
        target: "run-7f3a",
        from: "2026-11-01T09:30Z",
        to: "2026-11-02T00:00Z",
        cursor: "abc",
      }),
    ).toEqual({
      kind: "op.completed",
      actor: "a.kim",
      target: "run-7f3a",
      from: Date.UTC(2026, 10, 1, 9, 30),
      to: Date.UTC(2026, 10, 2),
      cursor: "abc",
    });
  });

  it("leaves out a time it cannot read rather than sending NaN", () => {
    const q = eventsQuery({ ...none, from: "yesterday-ish", to: "2026-11-02T00:00Z", cursor: "" });
    expect(q).toEqual({ to: Date.UTC(2026, 10, 2) });
    expect(parseInstant("not a date")).toBeUndefined();
    expect(parseInstant("")).toBeUndefined();
  });
});

describe("filterSearch", () => {
  it("drops the cursor whenever a filter changes, and clears unset params", () => {
    expect(filterSearch({ ...none, actor: " j.lee " })).toEqual({
      kind: "",
      actor: "j.lee",
      target: "",
      from: "",
      to: "",
      cursor: "",
    });
  });
});

describe("rangeProblem", () => {
  it("accepts an open or an ordered range", () => {
    expect(rangeProblem(none)).toBeNull();
    expect(rangeProblem({ ...none, from: "2026-11-01T00:00Z" })).toBeNull();
    expect(rangeProblem({ ...none, from: "2026-11-01T00:00Z", to: "2026-11-02T00:00Z" })).toBeNull();
  });

  it("names an unreadable time and a reversed range", () => {
    expect(rangeProblem({ ...none, from: "soon" })).toBe("The From time is not a date.");
    expect(rangeProblem({ ...none, to: "later" })).toBe("The To time is not a date.");
    expect(rangeProblem({ ...none, from: "2026-11-02T00:00Z", to: "2026-11-01T00:00Z" })).toBe(
      "From must be before To.",
    );
    expect(rangeProblem({ ...none, from: "2026-11-01T00:00Z", to: "2026-11-01T00:00Z" })).toBe(
      "From must be before To.",
    );
  });
});

describe("local time inputs", () => {
  it("round-trips a datetime-local value through the URL's UTC instant", () => {
    const local = "2026-11-01T09:30";
    const iso = fromLocalInput(local);
    expect(iso).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}Z$/);
    expect(toLocalInput(iso)).toBe(local);
    // The instant is the local wall-clock time, whatever the zone.
    expect(parseInstant(iso)).toBe(new Date(2026, 10, 1, 9, 30).getTime());
  });

  it("gives an empty value for an empty or unreadable one", () => {
    expect(fromLocalInput("")).toBe("");
    expect(fromLocalInput("09:30")).toBe("");
    expect(toLocalInput("")).toBe("");
    expect(toLocalInput("garbage")).toBe("");
  });
});

describe("isFiltered / emptyText", () => {
  it("says why the table is empty", () => {
    expect(isFiltered(none)).toBe(false);
    expect(isFiltered({ ...none, target: "orders-api" })).toBe(true);
    expect(emptyText(none, false)).toBe("No security events yet.");
    expect(emptyText({ ...none, kind: "passkey.reset" }, false)).toBe("No events match these filters.");
    expect(emptyText({ ...none, kind: "passkey.reset" }, true)).toBe("No older events.");
  });
});
