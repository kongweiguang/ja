// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later
/* eslint-disable react-refresh/only-export-components */

import { createBrowserRouter, type RouteObject, RouterProvider } from "react-router-dom";
import type { ReactElement } from "react";

/**
 * Routes remain injectable so the first desktop shell can evolve without
 * coupling the provider and sidecar lifecycle to a particular screen layout.
 */
export function createJaRouter(routes: RouteObject[]) {
  return createBrowserRouter(routes, { basename: "/" });
}

/**
 * The router component keeps navigation state inside React Router while Tauri
 * still owns the native window and process lifecycle.
 */
export function JaRouter({ router }: { router: ReturnType<typeof createJaRouter> }): ReactElement {
  return <RouterProvider router={router} />;
}
