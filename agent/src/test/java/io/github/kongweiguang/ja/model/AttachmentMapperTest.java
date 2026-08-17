// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;

import io.agentscope.core.message.ImageBlock;
import io.agentscope.core.message.TextBlock;
import io.github.kongweiguang.ja.profiles.CapabilitySet;
import io.github.kongweiguang.ja.profiles.ModelApi;
import io.github.kongweiguang.ja.profiles.ModelCapability;
import io.github.kongweiguang.ja.profiles.ModelProvider;
import java.util.EnumSet;
import java.util.List;
import org.junit.jupiter.api.Test;

/** Verifies MIME, quota, Unicode, and capability gates before AgentScope mapping. */
class AttachmentMapperTest {
    /** Uses all relevant capabilities to isolate mapping behavior from provider negotiation. */
    private static CapabilitySet all() {
        return new CapabilitySet(EnumSet.allOf(ModelCapability.class));
    }

    /** Maps text and image content through AgentScope's native block types. */
    @Test
    void mapsTextAndImage() {
        AttachmentMapper mapper = new AttachmentMapper(AttachmentPolicy.defaults());
        var blocks = mapper.map(List.of(
                Attachment.text("t", "note.md", "你好🙂"),
                new Attachment("i", "image.png", "IMAGE/PNG", new byte[] {1, 2, 3})), all());
        assertInstanceOf(TextBlock.class, blocks.get(0));
        assertEquals("你好🙂", ((TextBlock) blocks.get(0)).getText());
        assertInstanceOf(ImageBlock.class, blocks.get(1));
    }

    /** Rejects count, byte, MIME, invalid UTF-8, and missing image capability boundaries. */
    @Test
    void enforcesAttachmentLimitsAndCapabilities() {
        AttachmentPolicy policy = new AttachmentPolicy(1, 4, java.util.Set.of("text/plain"), java.util.Set.of("image/png"));
        AttachmentMapper mapper = new AttachmentMapper(policy);
        assertThrows(AttachmentMappingException.class, () -> mapper.map(List.of(
                Attachment.text("a", "a.txt", "a"), Attachment.text("b", "b.txt", "b")), all()));
        assertThrows(AttachmentMappingException.class, () -> mapper.map(List.of(
                new Attachment("a", "a.bin", "application/octet-stream", new byte[] {1})), all()));
        assertThrows(AttachmentMappingException.class, () -> mapper.map(List.of(
                new Attachment("a", "a.txt", "text/plain", new byte[] {(byte) 0xff})), all()));
        CapabilitySet textOnly = new CapabilitySet(java.util.Set.of(ModelCapability.TEXT));
        assertThrows(AttachmentMappingException.class, () -> mapper.map(List.of(
                new Attachment("i", "i.png", "image/png", new byte[] {1})), textOnly));
    }
}
