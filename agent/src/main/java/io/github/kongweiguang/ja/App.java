// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja;

import org.noear.solon.annotation.SolonMain;
import io.github.kongweiguang.ja.bootstrap.StdioApplication;

/**
 * Composition root only: Solon owns lifecycle/configuration and the bootstrap
 * adapter owns the protocol/runtime graph.
 */
@SolonMain
public final class App {
    private App() {
        // Prevent accidental construction because Solon owns application startup.
    }

    /**
     * Keeps the native/JVM entry point stable while all runtime wiring remains
     * in a separately testable bootstrap component.
     *
     * @param args process arguments supplied by the Tauri sidecar launcher
     */
    public static void main(String[] args) {
        int exitCode = new StdioApplication().run(args);
        if (exitCode != 0) {
            System.exit(exitCode);
        }
    }
}
