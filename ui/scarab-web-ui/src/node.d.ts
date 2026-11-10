// The no-DOM tests run under node, but tsconfig has `types: []`, so node's
// own types are not loaded. Declare only the little the tests use.
declare module "node:fs" {
  export function readFileSync(path: string | URL, encoding: "utf8"): string;
}
