// Minimal typing for the Vite env we read. The project sets `types: []`
// (no ambient libs), so we declare just the one flag rather than pulling in
// all of `vite/client`. VITE_SCARAB_MOCK=1 enables the fixture mode (see mock.ts).
interface ImportMetaEnv {
  readonly VITE_SCARAB_MOCK?: string;
}
interface ImportMeta {
  readonly env: ImportMetaEnv;
}
