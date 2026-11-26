import { describe, expect, it } from "vitest";
import { resultView } from "./result-value";

describe("resultView", () => {
  it("shows a plain value in full", () => {
    expect(resultView(214)).toEqual({ kind: "plain", text: "214" });
    expect(resultView("92.4%")).toEqual({ kind: "plain", text: "92.4%" });
    expect(resultView(false)).toEqual({ kind: "plain", text: "false" });
    expect(resultView(null)).toEqual({ kind: "plain", text: "null" });
  });

  it("previews an image result by its digest and name, and keeps the whole value", () => {
    const image = {
      "containerimage.config.digest": "sha256:4f1c",
      "containerimage.descriptor": { mediaType: "application/vnd.oci.image.manifest.v1+json", size: 1610 },
      "containerimage.digest": "sha256:9f86",
      "image.name": "registry.acme.example/scarab/server:sha-7c2e91d",
      digest: "sha256:9f86",
      layout: "/workspace/.scarab-build/5d41/oci",
    };
    const v = resultView(image);
    expect(v).toMatchObject({
      kind: "tree",
      size: "6 keys",
      preview: "digest: sha256:9f86 · image.name: registry.acme.example/scarab/server:sha-7c2e91d",
    });
    expect(v.kind === "tree" && JSON.parse(v.json)).toEqual(image);
  });

  it("previews an object with none of the usual keys in its own order, and a list by its items", () => {
    expect(resultView({ b: 1, nested: { x: 1 }, a: "two" })).toMatchObject({ size: "3 keys", preview: "b: 1 · a: two" });
    expect(resultView({ only: { x: 1 } })).toMatchObject({ size: "1 key", preview: "" });
    expect(resultView(["amd64", "arm64"])).toMatchObject({ size: "2 items", preview: "amd64, arm64" });
  });
});
