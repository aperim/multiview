import type { LinguiConfig } from "@lingui/conf";

// Lingui v5 catalog config. Compile-time ICU: `lingui extract` scans `t`/`<Trans>`
// macros into per-locale PO catalogs; `lingui compile` emits JS catalogs that the
// app lazy-loads. `ar` is a stub locale that also exercises the RTL path.
// See docs/web/internationalization.md.
const config: LinguiConfig = {
  locales: ["en", "ar", "pseudo"],
  sourceLocale: "en",
  pseudoLocale: "pseudo",
  fallbackLocales: {
    default: "en",
  },
  catalogs: [
    {
      path: "<rootDir>/src/locales/{locale}/messages",
      include: ["src"],
    },
  ],
  format: "po",
};

export default config;
