// Vite asset imports (tsconfig has `types: []`, so vite/client isn't loaded):
// SVGs resolve to their served URL; `?raw` text is used for the baked static
// ASCII marks from ui/brand/ascii. The animated scene JSONs type themselves
// via resolveJsonModule.
declare module "*.svg" {
  const url: string;
  export default url;
}

declare module "*.txt?raw" {
  const text: string;
  export default text;
}

// import.meta.glob (Vite) — declared here because tsconfig `types: []`
// deliberately skips vite/client.
interface ImportMeta {
  glob<T = unknown>(pattern: string, opts?: Record<string, unknown>): Record<string, T>;
}
