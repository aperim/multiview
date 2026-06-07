import { test, expect } from "@playwright/test";

// Drives the REAL built SPA and asserts the live system-metrics footer renders a
// CPU value plus a sparkline once `system.metrics` events stream over the
// WebSocket. The WS is mocked with `page.routeWebSocket` (Playwright 1.48+): the
// route handler never calls `connectToServer`, so the page's WebSocket is fully
// mocked, and we push Envelope frames the `useSystemMetrics` hook folds into the
// footer. Auth is reported "off" so the app renders without a login gate. REST
// is stubbed empty so unrelated queries resolve.

const SAMPLE = {
  cpu_util: 0.42,
  mem_used_bytes: 8_000_000_000,
  mem_total_bytes: 32_000_000_000,
  gpus: [
    {
      id: "gpu-0",
      vendor: "nvidia",
      name: "NVIDIA RTX 4060",
      compute_util: 0.6,
      mem_used_bytes: 4_000_000_000,
      mem_total_bytes: 8_000_000_000,
      encoder_util: 0.3,
      decoder_util: 0.2,
      encoder_sessions: 2,
      encoder_session_ceiling: 5,
    },
  ],
  program_fps: 50,
  sampled_hz: 2,
};

/** Build one `system.metrics` Envelope frame at the given sequence. */
function frame(seq: number, cpuUtil: number): string {
  return JSON.stringify({
    v: 1,
    t: "system.metrics",
    topic: "system",
    seq,
    ts: seq * 1_000_000,
    data: { ...SAMPLE, cpu_util: cpuUtil },
  });
}

test.beforeEach(async ({ page }) => {
  // Mock the realtime WS BEFORE navigation: accept the (mocked) connection and,
  // after the client opens it, push a few metrics frames. Not calling
  // connectToServer keeps the socket fully mocked (no real backend needed).
  await page.routeWebSocket("**/api/v1/ws*", (ws) => {
    ws.onMessage(() => {
      // The client may send a subscribe/heartbeat frame; we don't require it —
      // just keep the socket open and ignore client→server traffic.
    });
    // Push three samples with a rising CPU value so the footer + sparkline fill.
    ws.send(frame(1, 0.2));
    ws.send(frame(2, 0.42));
    ws.send(frame(3, 0.55));
  });

  // Auth off; everything else returns an empty JSON body.
  await page.route("**/api/v1/**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/auth/status*", (route) =>
    route.fulfill({ json: { auth_required: false, authenticated: true } }),
  );
});

test("the footer shows a live CPU value and a sparkline", async ({ page }) => {
  await page.goto("/");

  // The desktop footer is a button labelled for the system page.
  const footer = page.getByRole("button", {
    name: "Open the system metrics page",
  });
  await expect(footer).toBeVisible();

  // The hook conflates to the LATEST sample; the CPU cell shows the last
  // pushed value's locale percentage (0.55 -> 55%).
  await expect(footer.getByText("55%")).toBeVisible();

  // A sparkline graphic (an <svg role="img">) is rendered inside the footer.
  await expect(
    footer.getByRole("img", { name: "CPU utilisation trend" }),
  ).toBeVisible();

  // The live dot reports the open connection.
  await expect(footer.getByText("Live")).toBeVisible();
});

test("clicking the footer navigates to the System page", async ({ page }) => {
  await page.goto("/");

  await page
    .getByRole("button", { name: "Open the system metrics page" })
    .click();

  await expect(page).toHaveURL(/\/system$/);
  // The System page heading renders.
  await expect(
    page.getByRole("heading", { level: 1, name: "System" }),
  ).toBeVisible();
  // The first GPU card surfaces the device name.
  await expect(page.getByText("NVIDIA RTX 4060")).toBeVisible();
});
