import { test, expect } from "@playwright/test";

// Real-browser guard for the DEV-D3 cast flow: the Devices page hosts the cast
// panel (session list + Tier-D latency badge), the start sheet opens as a real
// Radix Dialog (focus trap / scroll lock — the failure class jsdom cannot
// reproduce), the manual host[:port] escape hatch posts the exact start body,
// and save-as-device posts the promotion body. API mocked via route
// interception; auth reported "off" so the app renders without a login gate.

const SESSIONS = [
  {
    id: "cast-session-1",
    name: "Lounge TV",
    address: "[fd00::20]:8009",
    output: "out-hls",
    media_url: "http://[fd00::7]:8080/hls/out-hls/index.m3u8",
    state: "ONLINE",
  },
];

const OUTPUTS = [
  {
    id: "out-hls",
    name: "Program HLS",
    body: { id: "out-hls", kind: "hls", path: "/hls/m", codec: "h264" },
  },
  {
    id: "out-rtsp",
    name: "RTSP mount",
    body: { id: "out-rtsp", kind: "rtsp_server", mount: "/multiview", codec: "h264" },
  },
];

const DEVICES = [
  {
    id: "tv-1",
    name: "Saved TV",
    body: { id: "tv-1", driver: "cast", address: "[fd00::21]:8009" },
  },
];

test.beforeEach(async ({ page }) => {
  // Catch-all FIRST so the specific routes registered after it take precedence
  // (Playwright matches most-recently-added routes first).
  await page.route("**/api/v1/**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/auth/status*", (route) =>
    route.fulfill({ json: { auth_required: false, authenticated: true } }),
  );
  await page.route("**/api/v1/devices", (route) => route.fulfill({ json: DEVICES }));
  await page.route("**/api/v1/outputs", (route) => route.fulfill({ json: OUTPUTS }));
  await page.route("**/api/v1/devices/cast-session-1/status", (route) =>
    route.fulfill({
      json: { device_id: "cast-session-1", state: "ONLINE", mode: "playing" },
    }),
  );
});

test("the cast panel lists the session and the sheet casts a manual address", async ({
  page,
}) => {
  let startBody: unknown;
  await page.route("**/api/v1/cast/sessions", (route) => {
    if (route.request().method() === "POST") {
      startBody = route.request().postDataJSON();
      return route.fulfill({
        status: 201,
        json: {
          id: "cast-session-2",
          name: "Bedroom",
          address: "[fd00::30]:8009",
          output: "out-hls",
          media_url: "http://[fd00::7]:8080/hls/out-hls/index.m3u8",
          state: "ADOPTING",
        },
      });
    }
    return route.fulfill({ json: SESSIONS });
  });

  await page.goto("/devices");
  const panel = page.getByTestId("cast-panel");
  await expect(panel).toBeVisible();
  await expect(panel.getByText("Lounge TV")).toBeVisible();
  // The lifecycle state and the honest Tier-D latency badge are text, never
  // colour alone.
  await expect(panel.getByText("Online")).toBeVisible();
  await expect(panel.getByText(/Tier D — 6–30 s behind live/)).toBeVisible();
  await page.screenshot({ path: "test-results/cast-session-list.png", fullPage: true });

  await panel.getByRole("button", { name: "Cast to a device…" }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toBeVisible();
  // The manual host[:port] escape hatch (cross-VLAN mDNS invisibility),
  // IPv6 bracketed first (ADR-0042).
  await dialog.getByLabel("Device address").fill("[fd00::30]:8009");
  await dialog.getByLabel("Session name (optional)").fill("Bedroom");
  await page.screenshot({ path: "test-results/cast-sheet.png" });
  await dialog.getByRole("button", { name: "Cast", exact: true }).click();

  await expect
    .poll(() => startBody, { message: "the start body should have been posted" })
    .toEqual({ address: "[fd00::30]:8009", name: "Bedroom", output: "out-hls" });
  // Scope to the toast notifications region: Radix Toast mirrors a toast's title
  // into a body-level off-screen `role="status"` aria-live announce span (for
  // screen readers) for a brief window after it mounts, so an unscoped
  // getByText("Casting started") transiently resolves to BOTH the visible title
  // and that 1px announce mirror — a strict-mode ambiguity. Both come from the
  // SAME single toast() call (verified in a real browser: the SPA notifies the
  // operator exactly once), so this scopes to the VISIBLE toast viewport rather
  // than weakening the assertion — a confirmation IS still required to show.
  await expect(
    page.getByRole("region").getByText("Casting started"),
  ).toBeVisible();
});

test("the session row shows the started-at readout once the LOAD was accepted (DEV-D3.1)", async ({
  page,
}) => {
  // started_unix_ns is the LOAD-accept stamp in Unix-epoch wall nanoseconds;
  // the panel ages it against wall time into an honest "started N … ago".
  const startedUnixNs = (Date.now() - 45_000) * 1_000_000; // ~45 s ago
  await page.route("**/api/v1/cast/sessions", (route) =>
    route.fulfill({
      json: [{ ...SESSIONS[0], started_unix_ns: startedUnixNs }],
    }),
  );

  await page.goto("/devices");
  const panel = page.getByTestId("cast-panel");
  await expect(panel.getByText("Lounge TV")).toBeVisible();
  // A real relative readout, text-only (never colour alone).
  await expect(panel.getByText(/started.*ago/i)).toBeVisible();
});

test("save-as-device posts the promotion body from the session row", async ({
  page,
}) => {
  await page.route("**/api/v1/cast/sessions", (route) =>
    route.fulfill({ json: SESSIONS }),
  );
  let saveBody: unknown;
  await page.route("**/api/v1/cast/sessions/cast-session-1/save", (route) => {
    saveBody = route.request().postDataJSON();
    return route.fulfill({
      status: 201,
      headers: { ETag: '"1"' },
      json: {
        id: "tv-lounge",
        name: "Lounge TV",
        body: { id: "tv-lounge", driver: "cast", address: "[fd00::20]:8009" },
      },
    });
  });

  await page.goto("/devices");
  await page
    .getByRole("button", { name: "Save as device: Lounge TV" })
    .click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toBeVisible();
  await dialog.getByLabel("Device identifier").fill("tv-lounge");
  await page.screenshot({ path: "test-results/cast-save-as-device.png" });
  await dialog.getByRole("button", { name: "Save device" }).click();

  await expect
    .poll(() => saveBody, { message: "the save body should have been posted" })
    .toEqual({ device_id: "tv-lounge", display_name: "Lounge TV" });
  // Scope to the toast notifications region for the same reason as the start
  // flow above: the Radix off-screen aria-live announce mirror makes an
  // unscoped getByText transiently ambiguous; the visible confirmation toast is
  // still asserted.
  await expect(
    page.getByRole("region").getByText("Saved as device"),
  ).toBeVisible();
});
