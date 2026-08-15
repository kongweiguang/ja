// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja;

import org.noear.solon.Solon;
import org.noear.solon.annotation.SolonMain;

/**
 * Keeps the Java sidecar composition root intentionally small until the
 * AgentScope harness and stdio protocol are wired into the production runtime.
 */
@SolonMain
public final class App {
    private App() {
        // Prevent accidental construction because Solon owns application startup.
    }

    /**
     * Lets Solon own lifecycle/configuration bootstrap so later agent services
     * can be registered without changing the native executable entry point.
     *
     * @param args process arguments supplied by the Tauri sidecar launcher
     */
    public static void main(String[] args) {
        Solon.start(App.class, args);
    }
}
