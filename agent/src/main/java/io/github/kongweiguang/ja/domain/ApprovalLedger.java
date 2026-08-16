// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/**
 * Compatibility name for callers that referred to the old concrete ledger.
 * It is an interface now so production cannot silently fall back to an
 * unbounded or process-local approval store.
 */
@Deprecated(forRemoval = false)
public interface ApprovalLedger extends ApprovalLedgerPort {
}
