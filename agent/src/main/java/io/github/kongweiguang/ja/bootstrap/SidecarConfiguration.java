// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.bootstrap;

import java.nio.ByteBuffer;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.Base64;
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
     * Parses the small command-line surface needed by the sidecar; the base64
     * transport keeps native Windows argv ASCII-safe while retaining the
     * legacy plain-path form for existing launchers.
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
            } else if (arg.startsWith("--data-dir-base64=")) {
                dataDirectory = parseBase64DataDirectory(
                        arg.substring("--data-dir-base64=".length()));
            } else if ("--data-dir-base64".equals(arg) && index + 1 < args.length) {
                dataDirectory = parseBase64DataDirectory(args[++index]);
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

    /**
     * Decodes an ASCII-only path transport with strict UTF-8 validation before
     * constructing a platform path; replacement decoding would silently
     * redirect a native launch to a different directory.
     */
    private static Path parseBase64DataDirectory(String value) {
        if (value == null || value.isBlank()) {
            throw new IllegalArgumentException("encoded data directory is required");
        }
        byte[] pathBytes;
        try {
            pathBytes = Base64.getUrlDecoder().decode(value);
        } catch (IllegalArgumentException exception) {
            throw new IllegalArgumentException("invalid encoded data directory", exception);
        }
        if (pathBytes.length == 0) {
            throw new IllegalArgumentException("encoded data directory is required");
        }
        String path;
        try {
            path = StandardCharsets.UTF_8.newDecoder()
                    .onMalformedInput(CodingErrorAction.REPORT)
                    .onUnmappableCharacter(CodingErrorAction.REPORT)
                    .decode(ByteBuffer.wrap(pathBytes))
                    .toString();
        } catch (CharacterCodingException exception) {
            throw new IllegalArgumentException("encoded data directory must be valid UTF-8",
                    exception);
        }
        return parseDataDirectory(path);
    }

    /** Returns whether the deterministic fixture runtime was explicitly enabled. */
    public boolean fakeRuntime() {
        return runtimeMode == RuntimeMode.FAKE;
    }
}
