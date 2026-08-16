// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.bootstrap;

import io.github.kongweiguang.ja.runtime.StdioRuntime;
import org.noear.solon.Solon;
import org.noear.solon.SolonApp;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

/** Solon lifecycle adapter that keeps the protocol loop outside the framework. */
public final class StdioApplication {
    private static final Logger LOGGER = LoggerFactory.getLogger(StdioApplication.class);

    /**
     * Starts Solon without HTTP, runs the blocking stdio loop, and uses the
     * explicit non-error stop variant required for a normal sidecar exit.
     */
    public int run(String[] args) {
        SolonApp app = null;
        StdioRuntime runtime = null;
        try {
            SidecarConfiguration configuration = SidecarConfiguration.fromArgs(args);
            app = Solon.start(io.github.kongweiguang.ja.App.class, args,
                    started -> started.enableHttp(false));
            runtime = new StdioRuntime(System.in, System.out, configuration);
            return runtime.run();
        } catch (RuntimeException exception) {
            LOGGER.error("JA sidecar failed ({} )", exception.getClass().getSimpleName());
            return 1;
        } finally {
            if (runtime != null) {
                runtime.close();
            }
            // Solon.stop() uses its default non-zero exit path; sidecars need a clean 0 exit.
            if (app != null) {
                Solon.stopBlock(false, 0);
            }
        }
    }
}
