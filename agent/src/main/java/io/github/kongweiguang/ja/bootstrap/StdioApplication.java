// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.bootstrap;

import io.github.kongweiguang.ja.runtime.StdioRuntime;
import io.github.kongweiguang.ja.runtime.TurnRuntime;
import org.noear.solon.Solon;
import org.noear.solon.SolonApp;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.time.Clock;
import java.util.function.Function;

/** Solon lifecycle adapter that keeps the protocol loop outside the framework. */
public final class StdioApplication {
    private static final Logger LOGGER = LoggerFactory.getLogger(StdioApplication.class);
    private final Function<SidecarConfiguration, TurnRuntime> turnRuntimeFactory;

    /** Lets StdioRuntime own deferred profile activation; no provider is constructed at launch. */
    public StdioApplication() {
        this(null);
    }

    /**
     * Injects the production graph at the composition boundary; the protocol
     * loop itself remains unchanged and testable without provider credentials.
     */
    public StdioApplication(Function<SidecarConfiguration, TurnRuntime> turnRuntimeFactory) {
        this.turnRuntimeFactory = turnRuntimeFactory;
    }

    /**
     * Starts Solon without HTTP, runs the blocking stdio loop, and uses the
     * explicit non-error stop variant required for a normal sidecar exit.
     */
    public int run(String[] args) {
        SolonApp app = null;
        StdioRuntime runtime = null;
        boolean aotProcessing = Boolean.getBoolean("solon.aot.processing");
        try {
            SidecarConfiguration configuration = SidecarConfiguration.fromArgs(args);
            app = Solon.start(io.github.kongweiguang.ja.App.class, args,
                    started -> started.enableHttp(false));
            if (aotProcessing) {
                // Solon's AOT processor invokes the application main method only to construct
                // AppContext metadata; blocking on stdin or stopping here would clear the very
                // context that SolonAotProcessor reads immediately afterwards.
                return 0;
            }
            if (turnRuntimeFactory == null) {
                // The default production constructor owns deferred profile activation and builds
                // the real AgentScope graph only after Rust resolves the selected secret.
                runtime = new StdioRuntime(System.in, System.out, configuration,
                        Clock.systemUTC());
            } else {
                // Injection is reserved for focused composition tests or an explicitly supplied
                // host graph; it must not replace the default activation path.
                TurnRuntime turnRuntime = configuration.fakeRuntime()
                        ? null : turnRuntimeFactory.apply(configuration);
                runtime = new StdioRuntime(System.in, System.out, configuration,
                        Clock.systemUTC(), turnRuntime);
            }
            return runtime.run();
        } catch (RuntimeException exception) {
            LOGGER.error("JA sidecar failed ({} )", exception.getClass().getSimpleName());
            return 1;
        } finally {
            if (runtime != null) {
                runtime.close();
            }
            // Solon.stop() uses its default non-zero exit path; sidecars need a clean 0 exit.
            if (app != null && !aotProcessing) {
                Solon.stopBlock(false, 0);
            }
        }
    }
}
