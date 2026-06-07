import { test, expect } from "@playwright/test";

// Regression guard for the renderer-OOM crash fixed in `ResourceTable`: opening a
// create / edit / delete Dialog over a POPULATED resource table must not drive
// `useReactTable` into an unbounded re-render loop. This only reproduces in a real
// browser, so it lives here (not in the jsdom suite). API is mocked via route
// interception; auth is reported "off" so the app renders without a login gate.

const SOURCES = [
  { id: "cam-1", name: "Camera 1", body: { id: "cam-1", kind: "rtsp", url: "rtsp://host/one" } },
  { id: "cam-2", name: "Camera 2", body: { id: "cam-2", kind: "bars" } },
];

test.beforeEach(async ({ page }) => {
  // Catch-all FIRST so the specific routes registered after it take precedence
  // (Playwright matches most-recently-added routes first).
  await page.route("**/api/v1/**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/auth/status*", (route) =>
    route.fulfill({ json: { auth_required: false, authenticated: true } }),
  );
  await page.route("**/api/v1/sources", (route) => route.fulfill({ json: SOURCES }));
  await page.route(/\/api\/v1\/sources\/[^/?]+$/, (route) =>
    route.fulfill({ json: SOURCES[0], headers: { ETag: '"1"' } }),
  );
});

test("create / edit / delete dialogs open without crashing the renderer", async ({ page }) => {
  const crashed: string[] = [];
  page.on("crash", () => crashed.push("renderer crashed"));

  await page.goto("/sources");
  await expect(page.getByText("Camera 1")).toBeVisible();

  // Create — opening this Dialog re-renders the page with the table mounted; the
  // old `data: [...rows]` drove an unbounded re-render loop that OOM-ed here.
  await page.getByRole("button", { name: "New source" }).click();
  await expect(page.getByRole("dialog")).toBeVisible({ timeout: 5000 });
  await page.keyboard.press("Escape");

  // Edit (loads the record via GET, prefills, opens the same Dialog).
  await page.getByRole("button", { name: "Edit source: Camera 1" }).click();
  await expect(page.getByRole("dialog")).toBeVisible({ timeout: 5000 });
  await page.keyboard.press("Escape");

  // Delete confirmation (a Dialog with no Select — also triggered the loop).
  await page.getByRole("button", { name: "Delete source: Camera 1" }).click();
  await expect(page.getByText("Delete this resource?")).toBeVisible({ timeout: 5000 });

  expect(crashed, crashed.join(",")).toHaveLength(0);
});
