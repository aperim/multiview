/*
 * English (source locale) message catalog.
 *
 * Compiled catalogs are normally emitted by `lingui compile` from extracted
 * `.po` files. For the source locale we ship an empty catalog: Lingui falls
 * back to the macro-provided source message, so `en` renders the authored
 * strings. Run `npm run i18n:extract && npm run i18n:compile` to regenerate.
 */
import type { Messages } from "@lingui/core";

export const messages: Messages = {};
