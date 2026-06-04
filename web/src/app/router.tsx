// The client-side route table (react-router v7). The shell (AppLayout) is the
// layout route; screens render into its <Outlet>.
import { createBrowserRouter } from "react-router-dom";

import { AppLayout } from "./AppLayout";
import { DashboardPage } from "../pages/DashboardPage";
import { LayoutsPage } from "../pages/LayoutsPage";
import { LayoutEditorPage } from "../pages/LayoutEditorPage";
import { NotFoundPage } from "../pages/NotFoundPage";
import { SettingsPage } from "../pages/SettingsPage";
import {
  OutputsPage,
  OverlaysPage,
  SourcesPage,
} from "../pages/SimplePages";

/** The application router. */
export const router = createBrowserRouter([
  {
    path: "/",
    element: <AppLayout />,
    children: [
      { index: true, element: <DashboardPage /> },
      { path: "layouts", element: <LayoutsPage /> },
      { path: "layouts/new", element: <LayoutEditorPage /> },
      { path: "layouts/:id", element: <LayoutEditorPage /> },
      { path: "sources", element: <SourcesPage /> },
      { path: "outputs", element: <OutputsPage /> },
      { path: "overlays", element: <OverlaysPage /> },
      { path: "settings", element: <SettingsPage /> },
      { path: "*", element: <NotFoundPage /> },
    ],
  },
]);
