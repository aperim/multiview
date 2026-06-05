// The in-app documentation table of contents. Labels are <Trans> elements
// (externalized for Lingui); the route paths are stable, non-localized
// identifiers under the SPA `/help` prefix.
//
// `/help` is used (not `/docs`) on purpose: the backend control plane serves the
// live API playground (Scalar) at the top-level `/docs` path, so the SPA keeps
// its in-app guide under `/help` to avoid clashing with that backend route.
import type { JSX } from "react";
import { Trans } from "@lingui/react/macro";
import {
  BookOpen,
  Boxes,
  Container,
  FileCog,
  Plug,
  Sparkles,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";

/** A single documentation table-of-contents entry. */
export interface DocsNavItem {
  /** Stable absolute route path (also used as a React key). */
  readonly path: string;
  /** Localized label element. */
  readonly label: JSX.Element;
  /** Short localized summary shown on the docs landing page. */
  readonly summary: JSX.Element;
  /** Leading icon (decorative; the label carries meaning). */
  readonly Icon: LucideIcon;
}

/** The documentation sections, in reading order. */
export const DOCS_NAV: readonly DocsNavItem[] = [
  {
    path: "/help",
    label: <Trans>Overview</Trans>,
    summary: (
      <Trans>What Multiview is, the design pillars, and how to get started.</Trans>
    ),
    Icon: BookOpen,
  },
  {
    path: "/help/containers",
    label: <Trans>Running in containers</Trans>,
    summary: (
      <Trans>
        docker run and docker compose, GPU access, volumes, healthchecks, and the
        API token.
      </Trans>
    ),
    Icon: Container,
  },
  {
    path: "/help/compose",
    label: <Trans>Compose reference</Trans>,
    summary: (
      <Trans>
        The services in the quick-start compose file and how to bring them up and
        down.
      </Trans>
    ),
    Icon: Boxes,
  },
  {
    path: "/help/config",
    label: <Trans>Config-as-code</Trans>,
    summary: (
      <Trans>
        The TOML schema: canvas, layout, sources, cells, overlays, and outputs.
      </Trans>
    ),
    Icon: FileCog,
  },
  {
    path: "/help/api",
    label: <Trans>API & realtime</Trans>,
    summary: (
      <Trans>
        The REST base, long-running operations, ETags, the WebSocket and SSE
        streams, and the live API playground.
      </Trans>
    ),
    Icon: Plug,
  },
  {
    path: "/help/features",
    label: <Trans>Feature guide</Trans>,
    summary: (
      <Trans>
        Layouts, sources, outputs, overlays, tally, salvos, and alarms — what each
        one is.
      </Trans>
    ),
    Icon: Sparkles,
  },
];
