// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

import io.agentscope.core.message.Base64Source;
import io.agentscope.core.message.ContentBlock;
import io.agentscope.core.message.ImageBlock;
import io.agentscope.core.message.TextBlock;
import io.github.kongweiguang.ja.profiles.CapabilitySet;
import io.github.kongweiguang.ja.profiles.ModelCapability;
import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Base64;
import java.util.List;
import java.util.Objects;

/** Maps bounded JA attachments to AgentScope's native text/image blocks. */
public final class AttachmentMapper {
    private final AttachmentPolicy policy;

    /** Captures quota policy once so a running request cannot observe a settings mutation. */
    public AttachmentMapper(AttachmentPolicy policy) {
        this.policy = Objects.requireNonNull(policy, "policy");
    }

    /** Validates every attachment before mapping any of them to avoid partially admitted requests. */
    public List<ContentBlock> map(List<Attachment> attachments, CapabilitySet capabilities) {
        Objects.requireNonNull(attachments, "attachments");
        Objects.requireNonNull(capabilities, "capabilities");
        if (attachments.size() > policy.maxCount()) {
            throw new AttachmentMappingException("attachment count exceeds configured limit");
        }
        long total = 0;
        List<ContentBlock> result = new ArrayList<>(attachments.size());
        for (Attachment attachment : attachments) {
            Objects.requireNonNull(attachment, "attachment");
            byte[] bytes = attachment.bytes();
            total = Math.addExact(total, bytes.length);
            if (total > policy.maxBytes()) {
                throw new AttachmentMappingException("attachment bytes exceed configured limit");
            }
            String mime = AttachmentPolicy.baseMime(attachment.mimeType());
            if (policy.textMimes().contains(mime)) {
                if (!capabilities.supports(ModelCapability.TEXT)) {
                    throw new AttachmentMappingException("model does not support text attachments");
                }
                result.add(TextBlock.builder().text(decodeUtf8(bytes)).build());
            } else if (policy.imageMimes().contains(mime)) {
                if (!capabilities.supports(ModelCapability.IMAGE)) {
                    throw new AttachmentMappingException("model does not support image attachments");
                }
                result.add(ImageBlock.builder()
                        .source(Base64Source.builder().mediaType(mime)
                                .data(Base64.getEncoder().encodeToString(bytes)).build())
                        .build());
            } else {
                throw new AttachmentMappingException("unsupported attachment MIME type");
            }
        }
        return List.copyOf(result);
    }

    /** Uses a strict decoder so invalid bytes cannot become replacement characters in prompts. */
    private static String decodeUtf8(byte[] bytes) {
        try {
            CharBuffer decoded = StandardCharsets.UTF_8.newDecoder()
                    .onMalformedInput(CodingErrorAction.REPORT)
                    .onUnmappableCharacter(CodingErrorAction.REPORT)
                    .decode(ByteBuffer.wrap(bytes));
            return decoded.toString();
        } catch (CharacterCodingException exception) {
            throw new AttachmentMappingException("text attachment is not valid UTF-8");
        }
    }
}
