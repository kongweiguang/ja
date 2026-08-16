// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Bounded nonblocking capture and safe event aggregation.

use super::redaction::{json_string_field, known_process, safe_operation, safe_process};
use super::{
    DIAGNOSTIC_MAX_BYTES, DIAGNOSTIC_MAX_EVENTS, DIAGNOSTIC_MAX_KEYS, DIAGNOSTIC_MAX_LINE_BYTES,
    DiagnosticKey, SandboxDenialDiagnostics, diagnostic_pump_allowed,
};
use std::io::{self, Read};

impl SandboxDenialDiagnostics {
    /// Drain both nonblocking streams within the global byte cap; stderr is
    /// consumed and classified only to prevent the helper from blocking.
    pub(crate) fn pump(&mut self) {
        if !diagnostic_pump_allowed(self.enabled, self.truncated, self.pipes_nonblocking) {
            return;
        }
        if let Some(mut stdout) = self.stdout.take() {
            self.pump_reader(&mut stdout, true);
            self.stdout = Some(stdout);
        }
        if !self.truncated
            && let Some(mut stderr) = self.stderr.take()
        {
            self.pump_reader(&mut stderr, false);
            self.stderr = Some(stderr);
        }
    }

    /// Convert a pipe read failure into a fixed diagnostic state while
    /// allowing the security cases to finish and report their own result.
    pub(super) fn pump_reader<R: Read>(&mut self, reader: &mut R, record: bool) {
        if self.drain_reader(reader, record).is_err() {
            self.read_error = Some("io");
            self.line_buffer.clear();
            self.truncated = true;
        }
    }

    /// Read only currently available bytes and split complete NDJSON lines
    /// without ever waiting on a log producer or retaining raw output.
    pub(super) fn drain_reader<R: Read>(&mut self, reader: &mut R, record: bool) -> io::Result<()> {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(read) => {
                    let remaining = DIAGNOSTIC_MAX_BYTES.saturating_sub(self.bytes);
                    if read > remaining {
                        self.bytes = DIAGNOSTIC_MAX_BYTES;
                        self.truncated = true;
                        return Ok(());
                    }
                    self.bytes += read;
                    if record {
                        self.append_record_bytes(&buffer[..read]);
                    }
                    if self.bytes >= DIAGNOSTIC_MAX_BYTES {
                        self.truncated = true;
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }

    /// Append complete records only while each unterminated line remains
    /// below its independent cap; overlong raw bytes are dropped at once.
    pub(super) fn append_record_bytes(&mut self, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() {
            let available = DIAGNOSTIC_MAX_LINE_BYTES.saturating_sub(self.line_buffer.len());
            if available == 0 {
                self.line_buffer.clear();
                self.truncated = true;
                return;
            }
            let remaining = &bytes[offset..];
            if let Some(newline) = remaining.iter().position(|byte| *byte == b'\n') {
                let record_length = newline + 1;
                if record_length > available {
                    self.line_buffer.clear();
                    self.truncated = true;
                    return;
                }
                self.line_buffer
                    .extend_from_slice(&remaining[..record_length]);
                offset += record_length;
                self.record_complete_lines();
                if self.truncated {
                    return;
                }
            } else {
                if remaining.len() > available {
                    self.line_buffer.clear();
                    self.truncated = true;
                    return;
                }
                self.line_buffer.extend_from_slice(remaining);
                return;
            }
        }
    }

    /// Convert complete log records into bounded whitelist counters.
    pub(super) fn record_complete_lines(&mut self) {
        while let Some(index) = self.line_buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.line_buffer.drain(..=index).collect();
            self.record_line(&line);
            if self.events >= DIAGNOSTIC_MAX_EVENTS {
                self.truncated = true;
                return;
            }
        }
    }

    /// Classify a final unterminated record without retaining it, because
    /// `log stream` may be killed between writes and its last line may not
    /// have reached a newline before the bounded cleanup deadline.
    pub(super) fn record_remaining_line(&mut self) {
        if !self.line_buffer.is_empty() && !self.truncated {
            let line = std::mem::take(&mut self.line_buffer);
            self.record_line(&line);
        }
    }

    /// Record only denial operation/category/process fields and discard all
    /// other NDJSON content, including timestamps and paths.
    pub(super) fn record_line(&mut self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        let lower = text.to_ascii_lowercase();
        if !(lower.contains("deny(") || lower.contains("\"deny\"") || lower.contains("violation")) {
            return;
        }
        if self.events >= DIAGNOSTIC_MAX_EVENTS {
            self.truncated = true;
            return;
        }
        self.events += 1;
        if self.counts.len() >= DIAGNOSTIC_MAX_KEYS {
            self.truncated = true;
            return;
        }
        let operation = safe_operation(json_string_field(&text, "operation"), &lower);
        let category = "sandbox-denial".to_string();
        let process = safe_process(
            json_string_field(&text, "process")
                .or_else(|| json_string_field(&text, "processName"))
                .or_else(|| known_process(&lower).map(str::to_owned)),
            &lower,
        );
        let key = DiagnosticKey {
            operation,
            category,
            process,
        };
        *self.counts.entry(key).or_insert(0) += 1;
    }
}
