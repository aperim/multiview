// SettingsPage "Configuration file" card (ADR-W020): shows whether the boot
// config file is being watched, the watched path, the last applied/rejected
// loads, and any restart-pending sections — read from
// GET /api/v1/config/watch-status.
// Plus the "Boot configuration" card (ADR-W022): the Boot/Loaded/Running
// divergence indicator and the confirm-gated revert-to-start / promote-to-boot
// actions.
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { MemoryRouter } from "react-router-dom";

import { SettingsPage } from "./SettingsPage";
import { I18nProvider as AppI18nProvider } from "../i18n/I18nProvider";
import { ThemeProvider } from "../theme/ThemeProvider";
import { renderWithProviders } from "../test/render";

const ACTIVE_STATUS = {
  active: true,
  path: "/etc/multiview/multiview.toml",
  applied_count: 3,
  last_applied: { at_ms: 1718000000123, detail: "sources: in_a changed" },
  last_rejected: { at_ms: 1718000050456, detail: "TOML parse error at line 3" },
  restart_pending: ["canvas", "outputs"],
};

const BOOT_MODEL = {
  modeled: true,
  boot_path: "/etc/multiview/multiview.toml",
  start: "boot",
  resumed: false,
  resume_fallback: null,
  diverged_from_loaded: ["layout", "sources"],
  diverged_from_boot_file: ["sources"],
  boot_file_error: null,
  active_path: "/etc/multiview/.multiview/active.toml",
  active_written_at_ms: 1718000060000,
};

const server = setupServer(
  http.get("*/api/v1/config/watch-status", () => HttpResponse.json(ACTIVE_STATUS)),
  http.get("*/api/v1/config/boot-model", () => HttpResponse.json(BOOT_MODEL)),
  // The Display Nodes card (DEV-B6) also mounts on this page; mock its reads
  // with empty lists so its queries never hit an unhandled request.
  http.get("*/api/v1/devices/enrollment-tokens", () => HttpResponse.json([])),
  http.get("*/api/v1/devices/pairing-requests", () => HttpResponse.json([])),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

function renderSettings(): void {
  renderWithProviders(
    <AppI18nProvider>
      <ThemeProvider>
        <MemoryRouter>
          <SettingsPage />
        </MemoryRouter>
      </ThemeProvider>
    </AppI18nProvider>,
  );
}

describe("SettingsPage configuration-file card", () => {
  it("shows the watched path and the watch-active state", async () => {
    renderSettings();
    // The card exists…
    expect(
      await screen.findByRole("heading", { name: /configuration file/i }),
    ).toBeInTheDocument();
    // …and reflects the live watch status from the API.
    expect(
      await screen.findByText("/etc/multiview/multiview.toml"),
    ).toBeInTheDocument();
    expect(await screen.findByText(/watching/i)).toBeInTheDocument();
  });

  it("surfaces the last applied and last rejected loads", async () => {
    renderSettings();
    expect(
      await screen.findByText(/sources: in_a changed/),
    ).toBeInTheDocument();
    expect(
      await screen.findByText(/TOML parse error at line 3/),
    ).toBeInTheDocument();
  });

  it("names the restart-pending sections", async () => {
    renderSettings();
    expect(await screen.findByText(/restart pending/i)).toBeInTheDocument();
    expect(await screen.findByText("canvas, outputs")).toBeInTheDocument();
  });

  it("shows the not-watched state when the watcher is inactive", async () => {
    server.use(
      http.get("*/api/v1/config/watch-status", () =>
        HttpResponse.json({
          active: false,
          path: null,
          applied_count: 0,
          last_applied: null,
          last_rejected: null,
          restart_pending: [],
        }),
      ),
    );
    renderSettings();
    expect(
      await screen.findByRole("heading", { name: /configuration file/i }),
    ).toBeInTheDocument();
    expect(await screen.findByText(/not watched/i)).toBeInTheDocument();
  });
});

describe("SettingsPage boot-configuration card (ADR-W022)", () => {
  it("shows the per-section divergence and both actions", async () => {
    renderSettings();
    expect(
      await screen.findByRole("heading", { name: /boot configuration/i }),
    ).toBeInTheDocument();
    // Divergence vs the startup (Loaded) snapshot names the sections…
    expect(
      await screen.findByText(/differs from the startup snapshot/i),
    ).toBeInTheDocument();
    expect(await screen.findByText("layout, sources")).toBeInTheDocument();
    // …and vs the boot file on disk.
    expect(
      await screen.findByText(/differs from the boot file/i),
    ).toBeInTheDocument();
    expect(
      await screen.findByRole("button", { name: /revert to start/i }),
    ).toBeInTheDocument();
    expect(
      await screen.findByRole("button", { name: /promote to boot/i }),
    ).toBeInTheDocument();
  });

  it("revert-to-start is confirm-gated and posts the action", async () => {
    let reverted = false;
    server.use(
      http.post("*/api/v1/config/revert-to-start", () => {
        reverted = true;
        return HttpResponse.json(
          {
            operation_id: "op-1",
            replayed: false,
            reverted: true,
            shed: 0,
            summary: ["sources: in_a changed"],
            restart_only: [],
          },
          { status: 202 },
        );
      }),
    );
    const user = userEvent.setup();
    renderSettings();

    await user.click(
      await screen.findByRole("button", { name: /revert to start/i }),
    );
    // Nothing posted before the confirmation.
    expect(reverted).toBe(false);
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toHaveTextContent(/revert to start/i);
    await user.click(screen.getByRole("button", { name: /^revert$/i }));
    expect(
      await screen.findByText(/reverted to the start configuration/i),
    ).toBeInTheDocument();
    expect(reverted).toBe(true);
  });

  it("a partial (shed) revert is reported honestly, never as reverted", async () => {
    // ADR-W022 review M4: a 202 with shed > 0 means the engine did NOT get
    // every command — the card must say "partially", not claim the revert.
    server.use(
      http.post("*/api/v1/config/revert-to-start", () =>
        HttpResponse.json(
          {
            operation_id: "op-2",
            replayed: false,
            reverted: false,
            shed: 2,
            summary: ["sources: in_a changed"],
            restart_only: [],
          },
          { status: 202 },
        ),
      ),
    );
    const user = userEvent.setup();
    renderSettings();

    await user.click(
      await screen.findByRole("button", { name: /revert to start/i }),
    );
    await screen.findByRole("dialog");
    await user.click(screen.getByRole("button", { name: /^revert$/i }));
    expect(
      await screen.findByText(/applied only partially/i),
    ).toBeInTheDocument();
    expect(
      screen.queryByText(/reverted to the start configuration/i),
    ).not.toBeInTheDocument();
  });

  it("promote-to-boot explains the file rewrite and posts after confirm", async () => {
    let promoted = false;
    server.use(
      http.post("*/api/v1/config/promote", () => {
        promoted = true;
        return HttpResponse.json({
          operation_id: "op-2",
          replayed: false,
          path: "/etc/multiview/multiview.toml",
          bytes: 1234,
          revision: 1,
        });
      }),
    );
    const user = userEvent.setup();
    renderSettings();

    await user.click(
      await screen.findByRole("button", { name: /promote to boot/i }),
    );
    expect(promoted).toBe(false);
    const dialog = await screen.findByRole("dialog");
    // The confirmation must say it rewrites the configuration file.
    expect(dialog).toHaveTextContent(/rewrites the boot configuration file/i);
    await user.click(screen.getByRole("button", { name: /^promote$/i }));
    expect(
      await screen.findByText(/promoted to the boot configuration file/i),
    ).toBeInTheDocument();
    expect(promoted).toBe(true);
  });

  it("is honest when the run has no boot model", async () => {
    server.use(
      http.get("*/api/v1/config/boot-model", () =>
        HttpResponse.json({
          modeled: false,
          boot_path: null,
          start: null,
          resumed: false,
          resume_fallback: null,
          diverged_from_loaded: [],
          diverged_from_boot_file: null,
          boot_file_error: null,
          active_path: null,
          active_written_at_ms: null,
        }),
      ),
    );
    renderSettings();
    expect(
      await screen.findByRole("heading", { name: /boot configuration/i }),
    ).toBeInTheDocument();
    expect(
      await screen.findByText(/not started from a configuration file/i),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /revert to start/i }),
    ).not.toBeInTheDocument();
  });
});
