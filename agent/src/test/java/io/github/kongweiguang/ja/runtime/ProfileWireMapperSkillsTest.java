// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;
import org.junit.jupiter.api.Test;

import java.util.List;

import static org.junit.jupiter.api.Assertions.assertEquals;

/** Verifies skill references survive the secret-free profile mapping boundary. */
final class ProfileWireMapperSkillsTest {
    private static final ObjectMapper JSON = new ObjectMapper();

    @Test
    void missingSkillReferencesMeanBuiltinOnly() {
        SavedProfile profile = ProfileWireMapper.parse(profile(null));

        assertEquals(List.of(), profile.skillRevisions());
    }

    @Test
    void selectedSkillReferencesAreRetainedAndWireProfileIsCopied() {
        SavedProfile profile = ProfileWireMapper.parse(profile(List.of("skill_selected")));

        assertEquals(List.of("skill_selected"), profile.skillRevisions());
        assertEquals("skill_selected", profile.wireProfile().path("skillRevisions")
                .get(0).textValue());
    }

    /** Builds only the model/profile fields needed to exercise this mapper's wire boundary. */
    private static ObjectNode profile(List<String> skillRevisions) {
        ObjectNode model = JSON.createObjectNode();
        model.put("provider", "openai");
        model.put("protocol", "openai_chat_completions");
        model.put("model", "fixture-model");
        ObjectNode profile = JSON.createObjectNode();
        profile.put("profileRevision", "profile_skills");
        profile.put("name", "Skills fixture");
        profile.put("accessMode", "workspace");
        profile.set("model", model);
        if (skillRevisions != null) {
            var values = JSON.createArrayNode();
            skillRevisions.forEach(values::add);
            profile.set("skillRevisions", values);
        }
        return profile;
    }
}
