// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import org.junit.jupiter.api.io.TempDir;

import java.nio.file.Path;
import java.time.Instant;

/** Shared explicit-path fixtures keep each SQLite test independently reopenable. */
abstract class PersistenceTestSupport {
    static final Instant START = Instant.parse("2026-08-17T00:00:00Z");

    @TempDir
    Path temp;

    /** Creates one database and one last-good backup path per test case. */
    final SqlitePersistenceConfig config(String name) {
        Path directory = temp.resolve(name);
        return SqlitePersistenceConfig.of(directory.resolve("ja.sqlite"),
                directory.resolve("ja.sqlite.backup"));
    }
}
