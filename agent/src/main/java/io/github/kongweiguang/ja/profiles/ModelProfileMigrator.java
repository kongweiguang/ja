// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;

/** Migrates old secret-free settings documents before strict profile construction. */
final class ModelProfileMigrator {
    private ModelProfileMigrator() {}

    /** Upgrades schema zero aliases only for the codec-owned parser tree. */
    static ObjectNode migrate(TrustedParsedProfile input) {
        try {
            if (!(input instanceof ModelProfileCodec.ParsedProfile)) {
                throw new ModelProfileReadException();
            }
            return migrateInternal(input);
        } catch (Throwable failure) {
            // VM termination is not recoverable; all input-node and callback failures stay product-redacted.
            if (failure instanceof VirtualMachineError error) {
                throw error;
            }
            throw new ModelProfileReadException();
        }
    }

    /** Fails closed for stale same-package callers instead of traversing an arbitrary JsonNode. */
    @Deprecated(forRemoval = true)
    static ObjectNode migrate(JsonNode input) {
        throw new ModelProfileReadException();
    }

    /** Copies only a fully validated standard tree so custom nodes cannot execute during deepCopy. */
    private static ObjectNode migrateInternal(TrustedParsedProfile input) {
        ModelProfileInputLimits.rejectSecretFields(input);
        JsonNode parsed = input.root();
        ObjectNode node = (ObjectNode) parsed.deepCopy();
        JsonNode versionNode = node.get("schemaVersion");
        int version = readVersion(versionNode);
        if (version > ModelProfile.CURRENT_SCHEMA_VERSION) {
            throw new IllegalArgumentException("unsupported future model profile schema");
        }
        if (version == 0) {
            rename(node, "apiMode", "api");
            rename(node, "modelName", "model");
            node.put("schemaVersion", ModelProfile.CURRENT_SCHEMA_VERSION);
        }
        return node;
    }

    /** Accepts only an integral, bounded JSON schema version; Jackson coercion is deliberately disabled. */
    private static int readVersion(JsonNode versionNode) {
        if (versionNode == null) {
            return 0;
        }
        if (!versionNode.isIntegralNumber() || !versionNode.canConvertToInt()) {
            throw new IllegalArgumentException("schemaVersion must be a bounded JSON integer");
        }
        int version = versionNode.intValue();
        if (version < 0 || version > ModelProfile.CURRENT_SCHEMA_VERSION) {
            throw new IllegalArgumentException("unsupported model profile schema version");
        }
        return version;
    }

    /** Renames only when the new name is absent so migration never overwrites explicit user data. */
    private static void rename(ObjectNode node, String oldName, String newName) {
        // Never overwrite a newer field because imports must preserve the user's explicit value.
        if (!node.has(newName) && node.has(oldName)) {
            node.set(newName, node.get(oldName));
        }
        node.remove(oldName);
    }

}
