// The shell's navigation model. Labels are <Trans> elements (externalized);
// the route paths are stable, non-localized identifiers.
import type { JSX } from "react";
import { Trans } from "@lingui/react/macro";
import {
  BookOpen,
  LayoutDashboard,
  LayoutGrid,
  Radio,
  Send,
  Layers,
  Settings,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";

/** A single primary navigation entry. */
export interface NavItem {
  /** Stable route path (also used as a React key). */
  readonly path: string;
  /** Localized label element. */
  readonly label: JSX.Element;
  /** Leading icon (decorative; the label carries meaning). */
  readonly Icon: LucideIcon;
}

/** The primary navigation, in display order. */
export const NAV_ITEMS: readonly NavItem[] = [
  { path: "/", label: <Trans>Dashboard</Trans>, Icon: LayoutDashboard },
  { path: "/layouts", label: <Trans>Layouts</Trans>, Icon: LayoutGrid },
  { path: "/sources", label: <Trans>Sources</Trans>, Icon: Radio },
  { path: "/outputs", label: <Trans>Outputs</Trans>, Icon: Send },
  { path: "/overlays", label: <Trans>Overlays</Trans>, Icon: Layers },
  { path: "/help", label: <Trans>Docs</Trans>, Icon: BookOpen },
  { path: "/settings", label: <Trans>Settings</Trans>, Icon: Settings },
];
