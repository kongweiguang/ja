// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import io.agentscope.core.state.AgentState;
import org.junit.jupiter.api.Test;

import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.sql.DriverManager;
import java.util.ArrayList;
import java.util.List;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/** Covers owner acquisition, one migration, backup publication, and basic recovery. */
class SqlitePersistenceLifecycleTest extends PersistenceTestSupport {
    /** AgentScope state remains available after closing and reopening the SQLite owner. */
    @Test
    void freshMigrateAndReopenPreserveAgentScopeState() {
        SqlitePersistenceConfig config = config("fresh");
        AgentState state = AgentState.builder().sessionId("session").summary("你好").build();
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            persistence.agentState().save(null, "session", "agent_state", state);
        }
        try (SqlitePersistence reopened = SqlitePersistence.open(config)) {
            AgentState restored = reopened.agentState()
                    .get(null, "session", "agent_state", AgentState.class).orElseThrow();
            assertEquals("你好", restored.getSummary());
        }
    }

    /** A checksum drift is rejected without replacing the last-good backup. */
    @Test
    void checksumDriftFailsClosedAndPreservesBackup() throws Exception {
        SqlitePersistenceConfig config = config("checksum");
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            assertTrue(!persistence.isClosed());
        }
        byte[] backup = Files.readAllBytes(config.backupPath());
        try (var connection = DriverManager.getConnection("jdbc:sqlite:" + config.databasePath());
             var statement = connection.createStatement()) {
            statement.executeUpdate("UPDATE schema_migrations SET checksum = '"
                    + "0".repeat(64) + "'");
        }
        PersistenceException failure = assertThrows(PersistenceException.class,
                () -> SqlitePersistence.open(config));
        assertEquals(PersistenceException.Code.MIGRATION_CHECKSUM, failure.code());
        assertArrayEquals(backup, Files.readAllBytes(config.backupPath()));
    }

    /** An unknown schema version fails closed and keeps the known-good backup. */
    @Test
    void tooNewSchemaFailsClosedAndPreservesBackup() throws Exception {
        SqlitePersistenceConfig config = config("too-new");
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            assertTrue(!persistence.isClosed());
        }
        byte[] backup = Files.readAllBytes(config.backupPath());
        try (var connection = DriverManager.getConnection("jdbc:sqlite:" + config.databasePath());
             var statement = connection.createStatement()) {
            statement.execute("PRAGMA user_version = 99");
        }
        PersistenceException failure = assertThrows(PersistenceException.class,
                () -> SqlitePersistence.open(config));
        assertEquals(PersistenceException.Code.SCHEMA_TOO_NEW, failure.code());
        assertArrayEquals(backup, Files.readAllBytes(config.backupPath()));
    }

    /** A failed follow-up migration keeps the prior backup available. */
    @Test
    void failedMigrationKeepsLastGoodBackup() throws Exception {
        SqlitePersistenceConfig config = config("migration-failure");
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            assertTrue(!persistence.isClosed());
        }
        byte[] backup = Files.readAllBytes(config.backupPath());
        List<SchemaMigration> migrations = new ArrayList<>(MigrationCatalog.production().migrations());
        migrations.add(SchemaMigration.fixed(3, "broken", "CREATE TABLE broken ("));
        MigrationCatalog broken = new MigrationCatalog(migrations);
        PersistenceException failure = assertThrows(PersistenceException.class,
                () -> SqlitePersistence.open(config, broken));
        assertEquals(PersistenceException.Code.MIGRATION, failure.code());
        assertArrayEquals(backup, Files.readAllBytes(config.backupPath()));
    }

    /** Corrupt primary bytes restore from the verified last-good backup. */
    @Test
    void corruptPrimaryRestoresVerifiedBackup() throws Exception {
        SqlitePersistenceConfig config = config("restore");
        AgentState state = AgentState.builder().sessionId("restore").build();
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            persistence.agentState().save(null, "restore", "agent_state", state);
        }
        Files.writeString(config.databasePath(), "not sqlite", StandardCharsets.UTF_8);
        try (SqlitePersistence reopened = SqlitePersistence.open(config)) {
            assertTrue(reopened.agentState().exists(null, "restore"));
        }
    }

    /** The sidecar lock prevents a second owner and is released after close. */
    @Test
    void secondOwnerIsRejectedAndReopenWorks() {
        SqlitePersistenceConfig config = config("lock");
        SqlitePersistence first = SqlitePersistence.open(config);
        try {
            PersistenceException failure = assertThrows(PersistenceException.class,
                    () -> SqlitePersistence.open(config));
            assertEquals(PersistenceException.Code.INSTANCE_LOCKED, failure.code());
        } finally {
            first.close();
        }
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            assertTrue(!persistence.isClosed());
        }
    }
}
