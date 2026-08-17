// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.bootstrap;

import java.nio.file.Path;
import java.util.Objects;

/**
 * Validated process options for the stdio sidecar.
 *
 * Keeping the runtime mode explicit prevents a development fake from being
 * accidentally selected by a production launcher or a missing environment
 * variable.
 */
public record SidecarConfiguration(RuntimeMode runtimeMode, Path dataDirectory) {
    /** The only runtime modes understood by the composition root. */
    public enum RuntimeMode {
        PRODUCTION,
        FAKE
    }

    /** Preserves an immutable mode so worker code cannot reinterpret argv. */
    public SidecarConfiguration {
        Objects.requireNonNull(runtimeMode, "runtimeMode");
        if (dataDirectory != null) {
            dataDirectory = dataDirectory.toAbsolutePath().normalize();
        }
    }

    /** Keeps focused callers source-compatible while production launchers opt into an explicit data directory. */
    public SidecarConfiguration(RuntimeMode runtimeMode) {
        this(runtimeMode, null);
    }

    /**
     * Parses only the deliberately small command-line surface needed by the
     * first sidecar; rejecting unknown flags avoids silent policy changes.
     */
    public static SidecarConfiguration fromArgs(String[] args) {
        RuntimeMode mode = RuntimeMode.PRODUCTION;
        Path dataDirectory = null;
        if (args == null) {
            return new SidecarConfiguration(mode, null);
        }
        for (int index = 0; index < args.length; index++) {
            String arg = Objects.requireNonNull(args[index], "args[" + index + "]");
            if ("--runtime=fake".equals(arg)) {
                mode = RuntimeMode.FAKE;
            } else if ("--runtime=production".equals(arg)) {
                mode = RuntimeMode.PRODUCTION;
            } else if ("--runtime".equals(arg) && index + 1 < args.length) {
                String value = args[++index];
                if ("fake".equals(value)) {
                    mode = RuntimeMode.FAKE;
                } else if ("production".equals(value)) {
                    mode = RuntimeMode.PRODUCTION;
                } else {
                    throw new IllegalArgumentException("unsupported runtime mode");
                }
            } else if (arg.startsWith("--data-dir=")) {
                dataDirectory = parseDataDirectory(arg.substring("--data-dir=".length()));
            } else if ("--data-dir".equals(arg) && index + 1 < args.length) {
                dataDirectory = parseDataDirectory(args[++index]);
            } else {
                throw new IllegalArgumentException("unsupported sidecar argument");
            }
        }
        return new SidecarConfiguration(mode, dataDirectory);
    }

    /** Rejects blank data paths before they can silently fall back to a repository or temp root. */
    private static Path parseDataDirectory(String value) {
        if (value == null || value.isBlank()) {
            throw new IllegalArgumentException("data directory is required");
        }
        return Path.of(value);
    }

    /** Returns whether the deterministic fixture runtime was explicitly enabled. */
    public boolean fakeRuntime() {
        return runtimeMode == RuntimeMode.FAKE;
    }
}
