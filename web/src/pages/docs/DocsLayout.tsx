// The documentation shell. Renders a secondary table-of-contents nav landmark
// plus the active topic page in an <Outlet>. Sits inside the app's <main>; the
// topic page owns the route's single <h1> via PageHeader.
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { NavLink, Outlet } from "react-router-dom";

import { cn } from "../../lib/utils";
import { DOCS_NAV } from "./docsNav";

/** Layout route for the in-app documentation under `/help`. */
export function DocsLayout(): JSX.Element {
  const { t } = useLingui();
  return (
    <div className="flex flex-col gap-6 lg:flex-row lg:items-start">
      <nav
        aria-label={t`Documentation`}
        className="shrink-0 lg:sticky lg:top-6 lg:w-60"
      >
        <p className="px-3 pb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
          <Trans>Documentation</Trans>
        </p>
        <ul className="flex flex-col gap-1">
          {DOCS_NAV.map(({ path, label, Icon }) => (
            <li key={path}>
              <NavLink
                to={path}
                end={path === "/help"}
                className={({ isActive }): string =>
                  cn(
                    "flex items-center gap-3 rounded-md px-3 py-2 text-sm font-medium transition-colors hover:bg-accent hover:text-accent-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
                    isActive
                      ? "bg-accent text-accent-foreground"
                      : "text-muted-foreground",
                  )
                }
              >
                {({ isActive }): JSX.Element => (
                  <>
                    <Icon className="size-4 shrink-0" aria-hidden="true" />
                    <span>{label}</span>
                    {isActive ? (
                      <span className="sr-only">(current)</span>
                    ) : null}
                  </>
                )}
              </NavLink>
            </li>
          ))}
        </ul>
      </nav>

      <div className="min-w-0 flex-1">
        <Outlet />
      </div>
    </div>
  );
}
