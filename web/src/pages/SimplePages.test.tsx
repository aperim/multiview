// SMOKE test: every resource dialog (create / edit / delete) renders over a
// populated table.
//
// NOTE — this does NOT guard the renderer-OOM re-render loop that the
// `ResourceTable` `data` memo fixes. That loop only reproduces in a REAL browser
// (it needs Radix Dialog's focus/scroll-lock effects + `ResizeObserver` to
// sustain the re-render cascade), which jsdom does not implement — verified: this
// test passes with OR without the fix. The actual regression guard is the
// Playwright e2e in `e2e/dialogs.spec.ts`, which drives a real chromium. This
// suite just keeps the dialog markup/forms from breaking.
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";

import { SourcesPage } from "./SimplePages";
import { renderWithProviders } from "../test/render";

const SOURCES = [
  { id: "cam-1", name: "Camera 1", body: { id: "cam-1", kind: "rtsp", url: "rtsp://host/one" } },
  { id: "cam-2", name: "Camera 2", body: { id: "cam-2", kind: "bars" } },
];

const server = setupServer(
  http.get("*/api/v1/sources", () => HttpResponse.json(SOURCES)),
  http.get("*/api/v1/sources/:id", ({ params }) => {
    const found = SOURCES.find((s) => s.id === String(params.id));
    return found
      ? HttpResponse.json(found, { headers: { ETag: '"1"' } })
      : new HttpResponse(null, { status: 404 });
  }),
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

describe("SourcesPage dialogs render over a populated table (smoke)", () => {
  it("opens the create dialog", async () => {
    renderWithProviders(<SourcesPage />);
    // Table populated from the list query (the loop needs rows present).
    expect(await screen.findByText("Camera 1")).toBeInTheDocument();
    // This click re-renders the page with the table mounted — must not hang.
    await userEvent.click(screen.getByRole("button", { name: "New source" }));
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toBeInTheDocument();
    expect(await screen.findByLabelText("Identifier")).toBeInTheDocument();
  });

  it("opens the delete confirmation", async () => {
    renderWithProviders(<SourcesPage />);
    expect(await screen.findByText("Camera 1")).toBeInTheDocument();
    await userEvent.click(
      screen.getByRole("button", { name: "Delete source: Camera 1" }),
    );
    expect(await screen.findByText("Delete this resource?")).toBeInTheDocument();
  });

  it("opens the edit dialog (loads the record)", async () => {
    renderWithProviders(<SourcesPage />);
    expect(await screen.findByText("Camera 1")).toBeInTheDocument();
    await userEvent.click(
      screen.getByRole("button", { name: "Edit source: Camera 1" }),
    );
    expect(await screen.findByRole("dialog")).toBeInTheDocument();
  });
});
