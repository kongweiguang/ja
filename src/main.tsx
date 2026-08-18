// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { createDevE2eProjectPicker } from "./app/e2eProjectPicker";

const devE2eProjectPicker = createDevE2eProjectPicker(import.meta.env);

// StrictMode keeps provider cleanup and future sidecar subscriptions honest
// during development without changing the production composition root.
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App projectPicker={devE2eProjectPicker} />
  </React.StrictMode>,
);
