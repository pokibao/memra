//! Governance: L0 identity schema change management.
//!
//! Implements the triple-approval mechanism:
//! 1. `propose_change` → insert pending record
//! 2. `approve_change` → increment approvals, auto-apply at threshold
//!    On the 3rd approval: writes an `identity_schema` row to `notes` AND
//!    flips `constitution_changes.status` to `'applied'` — all inside one txn.
//!
//! Ported from `backend/services/constitution.py`.

use std::collections::HashSet;

use chrono::Utc;
use rusqlite::Connection;
use tracing::{info, warn};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::storage::db::DbPool;
use crate::storage::writer::{NoteInsert, upsert_note, vector_to_blob};

/// Number of approvals required to apply a change.
const APPROVALS_NEEDED: i32 = 3;

/// Result of a propose or approve operation.
#[derive(Debug, serde::Serialize)]
pub struct ChangeRecord {
    pub id: String,
    pub change_type: String,
    pub proposed_content: String,
    pub reason: String,
    pub target_id: Option<String>,
    pub category: Option<String>,
    pub status: String,
    pub approvals_count: i32,
    pub approvals_needed: i32,
    pub approvals: Vec<ApprovalEntry>,
    pub proposer: String,
    pub created_at: String,
    pub updated_at: String,
    pub applied_at: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApprovalEntry {
    pub approver: String,
    #[serde(default)]
    pub actor_id: Option<String>,
    pub comment: Option<String>,
    pub approved_at: String,
}

/// Governance service for L0 constitution changes.
pub struct GovernanceService {
    db: DbPool,
    /// Project namespace applied to identity_schema rows on apply_change.
    /// Must match the project_id the read path (search_rules) filters on,
    /// otherwise approved L0 rows end up invisible under normal reads.
    project_id: Option<String>,
}

/// Return `true` for codepoints that look invisibly empty in normal
/// display and would otherwise let the same human register as two
/// distinct approvers.
///
/// Mirrors the Python `_is_default_ignorable` filter on PR #77 so the
/// two backends agree on what to strip. The PR #75 security review
/// flagged that the previous 4-codepoint allowlist (ZWSP/ZWNJ/ZWJ/BOM)
/// missed bidi controls (`\u{202E}` etc.), variation selectors, tag
/// characters, word joiner, soft hyphen, Mongolian vowel separator,
/// and combining grapheme joiner — each of which lets two visually
/// identical labels register as distinct approvers.
fn is_default_ignorable(c: char) -> bool {
    let cp = c as u32;
    matches!(
        c,
        '\u{200B}'  // ZWSP
        | '\u{200C}'  // ZWNJ
        | '\u{200D}'  // ZWJ
        | '\u{FEFF}'  // BOM / ZWNBSP
        | '\u{2060}'  // word joiner
        | '\u{00AD}'  // soft hyphen
        | '\u{180E}'  // Mongolian vowel separator
        | '\u{034F}'  // combining grapheme joiner
    ) || (0x202A..=0x202E).contains(&cp)        // LRE / RLE / PDF / LRO / RLO
        || (0x2066..=0x2069).contains(&cp)       // LRI / RLI / FSI / PDI
        || (0xFE00..=0xFE0F).contains(&cp)       // variation selectors
        || (0xE0100..=0xE01EF).contains(&cp)     // variation selectors supplement
        || (0xE0000..=0xE007F).contains(&cp) // tag characters
}

/// Canonical key used to deduplicate L0 approvers.
///
/// Matches Python `_canonical_approver_key` (backend/services/constitution.py
/// on PR #77) so the two red-line governance backends fold away identical
/// display-name variants:
///   * default-ignorable / format / control codepoints anywhere in the
///     string (zero-width family, BOM, bidi controls, word joiner, soft
///     hyphen, variation selectors, tag chars, combining grapheme joiner
///     — see [`is_default_ignorable`]),
///   * leading/trailing whitespace (incl. U+3000 ideographic space),
///   * NFKC-equivalent shapes: "Ａｌｉｃｅ" vs "Alice", composed vs
///     decomposed accented letters,
///   * case: "Alice" vs "alice" vs "ALICE".
///
/// Mixed-script homoglyphs (e.g. Cyrillic "Аlice") are **not** collapsed —
/// defeating them requires an authenticated actor_id, which the label-based
/// approver API does not expose. Same known gap as Python.
///
/// Known gap: Rust `.to_lowercase()` approximates Python `casefold()` for
/// the attack shapes we care about; true Unicode casefold (e.g. German "ß"
/// → "ss") would require an extra crate. Documented; same gap as Python.
fn canonicalize_approver(s: &str) -> String {
    s.chars()
        .filter(|c| !is_default_ignorable(*c))
        .collect::<String>()
        .nfkc()
        .collect::<String>()
        .trim()
        .to_lowercase()
}

impl GovernanceService {
    pub fn new(db: DbPool) -> Self {
        Self {
            db,
            project_id: None,
        }
    }

    /// Same as `new` but binds the service to a project namespace. Use this
    /// from the MCP server so `apply_change_inner` stamps the correct
    /// `project_id` on identity_schema rows.
    pub fn with_project(db: DbPool, project_id: impl Into<String>) -> Self {
        Self {
            db,
            project_id: Some(project_id.into()),
        }
    }

    /// Expose the underlying connection pool (used by tests to inspect the DB).
    pub fn pool(&self) -> &DbPool {
        &self.db
    }

    /// Ensure the constitution_changes table exists.
    pub fn init_table(&self) {
        self.db.with_conn(|conn| {
            if let Err(e) = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS constitution_changes (
                    id TEXT PRIMARY KEY,
                    change_type TEXT NOT NULL,
                    proposed_content TEXT NOT NULL,
                    reason TEXT NOT NULL,
                    target_id TEXT,
                    category TEXT,
                    status TEXT DEFAULT 'pending',
                    approvals_count INTEGER DEFAULT 0,
                    approvals_needed INTEGER DEFAULT 3,
                    approvals TEXT DEFAULT '[]',
                    proposer TEXT DEFAULT 'unknown',
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    applied_at TEXT
                )",
            ) {
                warn!("Failed to create constitution_changes table: {e}");
            }
        });
    }

    /// Propose a new L0 change.
    pub fn propose_change(
        &self,
        content: &str,
        reason: &str,
        change_type: Option<&str>,
        target_id: Option<&str>,
        category: Option<&str>,
        proposer: Option<&str>,
    ) -> Result<ChangeRecord, String> {
        let now = Utc::now().to_rfc3339();
        let change_id = Uuid::new_v4().to_string();
        let ct = change_type.unwrap_or("create");
        let prop = proposer.unwrap_or("mcp");

        self.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO constitution_changes
                 (id, change_type, proposed_content, reason, target_id, category,
                  status, approvals_count, approvals_needed, approvals, proposer,
                  created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 0, ?7, '[]', ?8, ?9, ?9)",
                rusqlite::params![
                    change_id,
                    ct,
                    content,
                    reason,
                    target_id,
                    category,
                    APPROVALS_NEEDED,
                    prop,
                    now,
                ],
            )
            .map_err(|e| format!("Insert failed: {e}"))?;

            info!("Proposed L0 change: {change_id}");

            Ok(ChangeRecord {
                id: change_id,
                change_type: ct.to_string(),
                proposed_content: content.to_string(),
                reason: reason.to_string(),
                target_id: target_id.map(String::from),
                category: category.map(String::from),
                status: "pending".to_string(),
                approvals_count: 0,
                approvals_needed: APPROVALS_NEEDED,
                approvals: vec![],
                proposer: prop.to_string(),
                created_at: now.clone(),
                updated_at: now,
                applied_at: None,
            })
        })
    }

    /// Approve a pending change. Returns updated record.
    ///
    /// When approvals reach threshold, status transitions directly to "applied"
    /// (skipping an intermediate "approved" state) because `apply_change_inner`
    /// writes the identity_schema row inside the same transaction.
    /// **Test-only convenience**: aliases `actor_id == approver_name`,
    /// which collapses the principal-vs-label dedup distinction and
    /// must NOT be used by production callers. The MCP service path at
    /// `memra-server/src/service.rs:222` always passes a transport-resolved
    /// principal to [`approve_change_for_principal`] instead.
    #[deprecated(
        note = "Production callers must call approve_change_for_principal with a server-resolved actor_id; this wrapper aliases label→principal and defeats per-principal dedup."
    )]
    pub fn approve_change(
        &self,
        change_id: &str,
        approver: Option<&str>,
        comment: Option<&str>,
    ) -> Result<ChangeRecord, String> {
        let approver_name = approver.unwrap_or("mcp");
        self.approve_change_for_principal(change_id, approver_name, Some(approver_name), comment)
    }

    /// Approve a pending change using a server-authenticated principal.
    ///
    /// `actor_id` is the authority-bearing identity resolved from transport
    /// credentials. `approver` is only a display/audit label.
    pub fn approve_change_for_principal(
        &self,
        change_id: &str,
        actor_id: &str,
        approver: Option<&str>,
        comment: Option<&str>,
    ) -> Result<ChangeRecord, String> {
        if actor_id.trim().is_empty() {
            return Err("Authenticated principal must not be empty".to_string());
        }

        let approver_name = approver.unwrap_or(actor_id);
        if approver_name.trim().is_empty() {
            return Err("approver must not be empty".to_string());
        }
        // The post-canonicalize check catches labels composed entirely
        // of default-ignorable codepoints (e.g. "\u{200B}\u{2060}") that
        // pass the raw `trim().is_empty()` guard but reduce to "" once
        // invisibles are stripped. Without this, two attackers passing
        // distinct invisible-only labels would dedupe to each other but
        // open the door for the empty key to swallow a future legitimate
        // approver whose label happens to fold to empty.
        if canonicalize_approver(approver_name).is_empty() {
            return Err("approver must not be empty after canonicalization".to_string());
        }

        let now = Utc::now().to_rfc3339();

        self.db.with_conn(|conn| {
            conn.execute_batch("BEGIN IMMEDIATE")
                .map_err(|e| format!("Transaction start failed: {e}"))?;

            let result =
                self.approve_inner(conn, change_id, actor_id, approver_name, comment, &now);

            match &result {
                Ok(_) => {
                    conn.execute_batch("COMMIT")
                        .map_err(|e| format!("Commit failed: {e}"))?;
                }
                Err(_) => {
                    let _ = conn.execute_batch("ROLLBACK");
                }
            }

            result
        })
    }

    fn approve_inner(
        &self,
        conn: &Connection,
        change_id: &str,
        actor_id: &str,
        approver: &str,
        comment: Option<&str>,
        now: &str,
    ) -> Result<ChangeRecord, String> {
        // Fetch current record
        let mut stmt = conn
            .prepare(
                "SELECT id, change_type, proposed_content, reason, target_id, category,
                        status, approvals_needed, approvals, proposer,
                        created_at, updated_at, applied_at
                 FROM constitution_changes WHERE id = ?1",
            )
            .map_err(|e| format!("Query prepare failed: {e}"))?;

        let record = stmt
            .query_row(rusqlite::params![change_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i32>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, Option<String>>(12)?,
                ))
            })
            .map_err(|e| format!("Change not found: {e}"))?;

        let (
            id,
            change_type,
            proposed_content,
            reason,
            target_id,
            category,
            status,
            approvals_needed,
            approvals_json,
            proposer,
            created_at,
            _updated_at,
            applied_at,
        ) = record;

        if status != "pending" {
            return Err(format!("Change is not pending (status: {status})"));
        }

        // Strict parse: a corrupted approvals JSON used to silently
        // collapse to an empty list, which would in turn re-emit
        // `approvals_count = 0` and erase every prior approval on the
        // next save. Surface the parse error and let the caller see
        // it; the BEGIN IMMEDIATE txn will roll back.
        let parsed_approvals: Vec<ApprovalEntry> = serde_json::from_str(&approvals_json)
            .map_err(|e| format!("Approvals JSON corrupted for change {id}: {e}"))?;

        // Treat the approvals JSON as source-of-truth and normalize any
        // historical duplicate rows before computing the next threshold count.
        let mut seen_principals = HashSet::new();
        let mut seen_approvers = HashSet::new();
        let mut approvals = Vec::with_capacity(parsed_approvals.len());
        for approval in parsed_approvals {
            let principal_key = approval
                .actor_id
                .as_deref()
                .unwrap_or(&approval.approver)
                .to_string();
            let approver_key = canonicalize_approver(&approval.approver);
            if !seen_principals.contains(&principal_key) && !seen_approvers.contains(&approver_key)
            {
                seen_principals.insert(principal_key);
                seen_approvers.insert(approver_key);
                approvals.push(approval);
            }
        }

        if seen_approvers.contains(&canonicalize_approver(approver)) {
            return Err(format!("already approved by this approver: {approver}"));
        }

        if seen_principals.contains(actor_id) {
            return Err(format!(
                "Authenticated principal already approved: {actor_id}"
            ));
        }

        approvals.push(ApprovalEntry {
            approver: approver.to_string(),
            actor_id: Some(actor_id.to_string()),
            comment: comment.map(String::from),
            approved_at: now.to_string(),
        });

        let new_count = approvals.len() as i32;
        let threshold_reached = new_count >= approvals_needed;

        let approvals_str = serde_json::to_string(&approvals).unwrap_or_else(|_| "[]".to_string());

        // When the threshold is reached, apply_change_inner will set
        // status='applied' and applied_at=now.  For intermediate approvals
        // we just record count + approvals JSON (status stays 'pending').
        if threshold_reached {
            // Phase 1: persist the new approvals JSON + count (status still
            // 'pending' at this point — the txn has not committed yet).
            conn.execute(
                "UPDATE constitution_changes
                 SET approvals_count = ?1, approvals = ?2, updated_at = ?3
                 WHERE id = ?4",
                rusqlite::params![new_count, approvals_str, now, change_id],
            )
            .map_err(|e| format!("Approval count update failed: {e}"))?;

            // Phase 2: apply the change — writes identity_schema row + flips
            // status='applied'.  If this errors, the entire transaction
            // (started in approve_change_for_principal) is rolled back.
            self.apply_change_inner(
                conn,
                change_id,
                &change_type,
                &proposed_content,
                target_id.as_deref(),
                category.as_deref(),
                now,
                actor_id,
            )?;

            info!(
                "Applied L0 change {change_id}: {new_count}/{approvals_needed} (status: applied)"
            );

            Ok(ChangeRecord {
                id,
                change_type,
                proposed_content,
                reason,
                target_id,
                category,
                status: "applied".to_string(),
                approvals_count: new_count,
                approvals_needed,
                approvals,
                proposer,
                created_at,
                updated_at: now.to_string(),
                applied_at: Some(now.to_string()),
            })
        } else {
            conn.execute(
                "UPDATE constitution_changes
                 SET approvals_count = ?1, approvals = ?2, status = 'pending', updated_at = ?3
                 WHERE id = ?4",
                rusqlite::params![new_count, approvals_str, now, change_id],
            )
            .map_err(|e| format!("Update failed: {e}"))?;

            info!(
                "Approved L0 change {change_id}: {new_count}/{approvals_needed} (status: pending)"
            );

            Ok(ChangeRecord {
                id,
                change_type,
                proposed_content,
                reason,
                target_id,
                category,
                status: "pending".to_string(),
                approvals_count: new_count,
                approvals_needed,
                approvals,
                proposer,
                created_at,
                updated_at: now.to_string(),
                applied_at,
            })
        }
    }

    /// Apply an approved L0 change to the `notes` table as an `identity_schema`
    /// row and flip `constitution_changes.status` to `'applied'`.
    ///
    /// MUST be called inside the same transaction as the approval count update.
    /// Any error here causes the caller to roll back the entire transaction so
    /// `status` stays `'pending'` and no partial write lands.
    #[allow(clippy::too_many_arguments)]
    fn apply_change_inner(
        &self,
        conn: &Connection,
        change_id: &str,
        change_type: &str,
        proposed_content: &str,
        target_id: Option<&str>,
        category: Option<&str>,
        now: &str,
        applied_by: &str,
    ) -> Result<(), String> {
        // Hand-formatted JSON only escaped `"` and would have allowed
        // `\` / control / newline injection (security review on PR #75).
        // Route through serde_json so the encoder owns the contract.
        let metadata_json = serde_json::to_string(&serde_json::json!({
            "applied_by": applied_by,
            "change_id": change_id,
        }))
        .map_err(|e| format!("apply_change metadata encode failed: {e}"))?;

        match change_type {
            "create" | "update" => {
                let note_id = if change_type == "update" {
                    let tid = target_id
                        .ok_or_else(|| "apply_change update requires target_id".to_string())?;
                    let exists: i64 = conn
                        .query_row(
                            "SELECT COUNT(*) FROM notes
                             WHERE id = ?1 AND layer = 'identity_schema'",
                            rusqlite::params![tid],
                            |row| row.get(0),
                        )
                        .map_err(|e| format!("apply_change update precheck failed: {e}"))?;
                    if exists == 0 {
                        return Err(format!(
                            "apply_change update: target identity_schema row \
                             not found for id={tid}; refusing to upsert a \
                             new row under an update change_type"
                        ));
                    }
                    tid.to_string()
                } else {
                    Uuid::new_v4().to_string()
                };

                let embedding = crate::embedding::embed_text(proposed_content);
                let (vector_json, vector_blob) = match embedding {
                    Some(ref emb) => {
                        let json = serde_json::to_string(emb)
                            .map_err(|e| format!("apply_change vector encode failed: {e}"))?;
                        (Some(json), Some(vector_to_blob(emb)))
                    }
                    None => (None, None),
                };

                let note = NoteInsert {
                    id: note_id,
                    content: proposed_content.to_string(),
                    layer: "identity_schema".to_string(),
                    category: category.map(String::from),
                    is_active: true,
                    confidence: Some(0.99),
                    // W5-B (Codex Crit 2): stamp project_id so the read path
                    // (which filters on notes.project_id) can actually see
                    // approved L0 rows. Fall back to None only when the
                    // service was constructed without a namespace (legacy
                    // `GovernanceService::new(db)` path, e.g. some tests).
                    project_id: self.project_id.clone(),
                    created_at: Some(now.to_string()),
                    updated_at: Some(now.to_string()),
                    metadata_json: Some(metadata_json),
                    vector_json,
                    vector_blob,
                    is_head: true,
                    evolution_state: Some("active".to_string()),
                    agent: Some(applied_by.to_string()),
                    ..Default::default()
                };

                upsert_note(conn, &note).map_err(|e| format!("apply_change upsert failed: {e}"))?;
            }
            "delete" => {
                let tid = target_id.ok_or_else(|| "delete requires target_id".to_string())?;

                let rows_affected = conn
                    .execute(
                        "UPDATE notes
                         SET is_active = 0,
                             updated_at = ?1,
                             evolution_state = 'deprecated'
                         WHERE id = ?2 AND layer = 'identity_schema'",
                        rusqlite::params![now, tid],
                    )
                    .map_err(|e| format!("apply_change delete failed: {e}"))?;

                // Treat 0 rows affected as an error so the transaction rolls back.
                if rows_affected == 0 {
                    return Err(format!(
                        "apply_change delete: no identity_schema row found with id={tid}"
                    ));
                }
            }
            other => {
                return Err(format!("apply_change: unknown change_type '{other}'"));
            }
        }

        // Flip status to 'applied' — inside the same transaction.
        conn.execute(
            "UPDATE constitution_changes
             SET status = 'applied', applied_at = ?1, updated_at = ?1
             WHERE id = ?2",
            rusqlite::params![now, change_id],
        )
        .map_err(|e| format!("apply_change status flip failed: {e}"))?;

        Ok(())
    }
}

#[cfg(test)]
#[allow(deprecated)] // tests intentionally exercise the legacy `approve_change` wrapper
mod tests {
    use super::*;
    use crate::storage::db::DbPool;

    fn test_gov() -> GovernanceService {
        let pool = DbPool::open(std::path::Path::new(":memory:")).unwrap();
        // Create notes tables so apply_change_inner can upsert identity_schema rows.
        pool.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS notes (
                    id                TEXT PRIMARY KEY,
                    content           TEXT NOT NULL,
                    layer             TEXT,
                    category          TEXT,
                    is_active         INTEGER NOT NULL DEFAULT 1,
                    confidence        REAL,
                    agent_id          TEXT,
                    project_id        TEXT,
                    created_at        TEXT,
                    updated_at        TEXT,
                    valid_at          REAL,
                    metadata_json     TEXT,
                    vector_json       TEXT,
                    vector_blob       BLOB,
                    evolution_state   TEXT DEFAULT 'active',
                    topic_key         TEXT,
                    is_head           INTEGER DEFAULT 1,
                    review_after      TEXT,
                    room              TEXT,
                    agent             TEXT,
                    difficulty        INTEGER,
                    time_cost_hint    TEXT,
                    related_ids_json  TEXT,
                    role              TEXT,
                    session_id        TEXT,
                    source            TEXT,
                    created_by        TEXT,
                    version           INTEGER DEFAULT 1,
                    root_id           TEXT,
                    cold_storage_ref  TEXT,
                    event_when        TEXT,
                    event_when_ts     REAL
                );
                CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts
                    USING fts5(note_id UNINDEXED, content);",
            )
            .expect("notes schema");
        });
        let gov = GovernanceService::new(pool);
        gov.init_table();
        gov
    }

    #[test]
    fn propose_creates_pending_record() {
        let gov = test_gov();
        let record = gov
            .propose_change("New identity rule", "Testing", None, None, None, None)
            .unwrap();

        assert_eq!(record.status, "pending");
        assert_eq!(record.approvals_count, 0);
        assert_eq!(record.approvals_needed, 3);
        assert_eq!(record.proposed_content, "New identity rule");
    }

    #[test]
    fn approve_increments_count() {
        let gov = test_gov();
        let record = gov
            .propose_change("Rule A", "Test", None, None, None, None)
            .unwrap();

        let approved = gov
            .approve_change(&record.id, Some("alice"), Some("looks good"))
            .unwrap();
        assert_eq!(approved.approvals_count, 1);
        assert_eq!(approved.status, "pending");

        let approved = gov.approve_change(&record.id, Some("bob"), None).unwrap();
        assert_eq!(approved.approvals_count, 2);
        assert_eq!(approved.status, "pending");
    }

    #[test]
    fn third_approval_triggers_applied_status() {
        let gov = test_gov();
        let record = gov
            .propose_change("Rule B", "Test", None, None, None, None)
            .unwrap();

        gov.approve_change(&record.id, Some("alice"), None).unwrap();
        gov.approve_change(&record.id, Some("bob"), None).unwrap();
        let final_record = gov
            .approve_change(&record.id, Some("charlie"), None)
            .unwrap();

        assert_eq!(final_record.approvals_count, 3);
        // Status must be 'applied', not 'approved' — the apply step runs inside
        // the same transaction and flips directly to 'applied'.
        assert_eq!(final_record.status, "applied");
        assert_eq!(final_record.approvals.len(), 3);
        assert!(
            final_record.applied_at.is_some(),
            "applied_at should be set after 3rd approval"
        );
        let (has_vector_json, blob_len): (i64, i64) = gov
            .db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT vector_json IS NOT NULL, length(vector_blob)
                     FROM notes
                     WHERE layer = 'identity_schema' AND content = ?1",
                    ["Rule B"],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
            })
            .unwrap();
        assert_eq!(has_vector_json, 1);
        assert_eq!(blob_len, crate::embedding::EMBEDDING_BLOB_BYTES as i64);
    }

    #[test]
    fn duplicate_approver_rejected() {
        let gov = test_gov();
        let record = gov
            .propose_change("Rule C", "Test", None, None, None, None)
            .unwrap();

        gov.approve_change(&record.id, Some("alice"), None).unwrap();
        let err = gov
            .approve_change(&record.id, Some("alice"), None)
            .unwrap_err();
        assert!(
            err.contains("already approved"),
            "Expected duplicate error: {err}"
        );
    }

    #[test]
    fn duplicate_authenticated_principal_rejected_even_with_different_labels() {
        let gov = test_gov();
        let record = gov
            .propose_change("Rule C2", "Test", None, None, None, None)
            .unwrap();

        gov.approve_change_for_principal(&record.id, "api-key-1", Some("alice"), None)
            .unwrap();
        let err = gov
            .approve_change_for_principal(&record.id, "api-key-1", Some("bob"), None)
            .unwrap_err();

        assert!(
            err.contains("already approved"),
            "Expected duplicate principal error: {err}"
        );
    }

    #[test]
    fn duplicate_display_approver_rejected_even_with_different_principals() {
        let gov = test_gov();
        let record = gov
            .propose_change("Rule C3", "Test", None, None, None, None)
            .unwrap();

        let first = gov
            .approve_change_for_principal(&record.id, "actor-1", Some("alice"), None)
            .unwrap();
        assert_eq!(first.approvals_count, 1);

        for actor_id in ["actor-2", "actor-3"] {
            let err = gov
                .approve_change_for_principal(&record.id, actor_id, Some("alice"), None)
                .unwrap_err();
            assert!(
                err.contains("already approved by this approver"),
                "Expected duplicate approver error: {err}"
            );
        }

        let (stored_count, stored_status, approvals_json): (i32, String, String) =
            gov.pool().with_conn(|conn| {
                conn.query_row(
                    "SELECT approvals_count, status, approvals
                     FROM constitution_changes WHERE id = ?1",
                    [&record.id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("constitution change row")
            });
        let approvals: Vec<ApprovalEntry> =
            serde_json::from_str(&approvals_json).expect("approval JSON");

        assert_eq!(stored_count, 1);
        assert_eq!(stored_status, "pending");
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0].approver, "alice");

        gov.approve_change_for_principal(&record.id, "actor-4", Some("bob"), None)
            .unwrap();
        let final_record = gov
            .approve_change_for_principal(&record.id, "actor-5", Some("carol"), None)
            .unwrap();

        assert_eq!(final_record.approvals_count, 3);
        assert_eq!(final_record.status, "applied");
    }

    #[test]
    fn legacy_duplicate_approval_rows_are_normalized_on_next_approval() {
        let gov = test_gov();
        let record = gov
            .propose_change("Rule C4", "Test", None, None, None, None)
            .unwrap();

        let dirty_approvals = vec![
            ApprovalEntry {
                approver: "alice".to_string(),
                actor_id: Some("actor-1".to_string()),
                comment: None,
                approved_at: "legacy-ts-1".to_string(),
            },
            ApprovalEntry {
                approver: "alice".to_string(),
                actor_id: Some("actor-2".to_string()),
                comment: None,
                approved_at: "legacy-ts-2".to_string(),
            },
        ];
        let dirty_approvals_json = serde_json::to_string(&dirty_approvals).unwrap();
        gov.pool().with_conn(|conn| {
            conn.execute(
                "UPDATE constitution_changes
                 SET approvals_count = 2, approvals = ?1
                 WHERE id = ?2",
                rusqlite::params![dirty_approvals_json, &record.id],
            )
            .expect("seed dirty approvals");
        });

        let second = gov
            .approve_change_for_principal(&record.id, "actor-2", Some("bob"), None)
            .unwrap();
        assert_eq!(second.approvals_count, 2);
        assert_eq!(second.status, "pending");
        assert_eq!(second.approvals.len(), 2);
        assert_eq!(
            second
                .approvals
                .iter()
                .map(|entry| entry.approver.as_str())
                .collect::<Vec<_>>(),
            vec!["alice", "bob"]
        );

        let (stored_count, approvals_json): (i32, String) = gov.pool().with_conn(|conn| {
            conn.query_row(
                "SELECT approvals_count, approvals
                 FROM constitution_changes WHERE id = ?1",
                [&record.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("constitution change row")
        });
        let stored_approvals: Vec<ApprovalEntry> =
            serde_json::from_str(&approvals_json).expect("approval JSON");
        assert_eq!(stored_count, 2);
        assert_eq!(stored_approvals.len(), 2);

        let final_record = gov
            .approve_change_for_principal(&record.id, "actor-3", Some("carol"), None)
            .unwrap();
        assert_eq!(final_record.approvals_count, 3);
        assert_eq!(final_record.status, "applied");
    }

    #[test]
    fn empty_actor_or_approver_rejected() {
        let gov = test_gov();
        let record = gov
            .propose_change("Rule C5", "Test", None, None, None, None)
            .unwrap();

        let err = gov
            .approve_change_for_principal(&record.id, "", Some("alice"), None)
            .unwrap_err();
        assert!(
            err.contains("principal must not be empty"),
            "Expected empty principal error: {err}"
        );

        let err = gov
            .approve_change_for_principal(&record.id, "actor-1", Some(" "), None)
            .unwrap_err();
        assert!(
            err.contains("approver must not be empty"),
            "Expected empty approver error: {err}"
        );
    }

    #[test]
    fn approve_non_pending_rejected() {
        let gov = test_gov();
        let record = gov
            .propose_change("Rule D", "Test", None, None, None, None)
            .unwrap();

        // Get to applied state (3rd approval triggers apply_change_inner).
        gov.approve_change(&record.id, Some("a"), None).unwrap();
        gov.approve_change(&record.id, Some("b"), None).unwrap();
        gov.approve_change(&record.id, Some("c"), None).unwrap();

        // A 4th approve must fail because status is now 'applied', not 'pending'.
        let err = gov.approve_change(&record.id, Some("d"), None).unwrap_err();
        assert!(err.contains("not pending"), "Expected status error: {err}");
    }

    #[test]
    fn nonexistent_change_rejected() {
        let gov = test_gov();
        let err = gov
            .approve_change("nonexistent-id", Some("alice"), None)
            .unwrap_err();
        assert!(err.contains("not found"), "Expected not found error: {err}");
    }

    #[test]
    fn approver_display_name_canonicalized_before_dedup() {
        let gov = test_gov();
        let record = gov
            .propose_change(
                "Rule canon",
                "Test canonicalization",
                None,
                None,
                None,
                None,
            )
            .unwrap();

        // First: "alice" approves successfully
        gov.approve_change_for_principal(&record.id, "actor-1", Some("alice"), None)
            .unwrap();

        // Variants of "alice" must all be rejected
        for (actor_id, variant) in &[
            ("actor-2", "Alice"),
            ("actor-3", " alice"),
            ("actor-4", "alice "),
            ("actor-5", "ALICE"),
            ("actor-6", " Alice "),
        ] {
            let err = gov
                .approve_change_for_principal(&record.id, actor_id, Some(variant), None)
                .unwrap_err();
            assert!(
                err.contains("already approved by this approver"),
                "Variant {variant:?} should be rejected: got {err}"
            );
        }

        // A genuinely distinct approver still works
        gov.approve_change_for_principal(&record.id, "actor-7", Some("bob"), None)
            .unwrap();
    }

    #[test]
    fn approver_fullwidth_and_ascii_dedupe_to_same_key() {
        let gov = test_gov();
        let record = gov
            .propose_change("Rule fullwidth", "NFKC coverage", None, None, None, None)
            .unwrap();

        gov.approve_change_for_principal(&record.id, "actor-1", Some("Alice"), None)
            .unwrap();

        // Full-width "Ａｌｉｃｅ" (U+FF21..) must be treated as the same approver
        let err = gov
            .approve_change_for_principal(&record.id, "actor-2", Some("Ａｌｉｃｅ"), None)
            .unwrap_err();
        assert!(
            err.contains("already approved by this approver"),
            "Fullwidth Ａｌｉｃｅ should map to Alice: got {err}"
        );
    }

    #[test]
    fn approver_with_zero_width_chars_dedupe() {
        let gov = test_gov();
        let record = gov
            .propose_change(
                "Rule ZWSP",
                "Invisible char coverage",
                None,
                None,
                None,
                None,
            )
            .unwrap();

        gov.approve_change_for_principal(&record.id, "actor-1", Some("alice"), None)
            .unwrap();

        for variant in &[
            "Al\u{200B}ice",
            "\u{200B}alice",
            "al\u{200C}ice",
            "alice\u{200D}",
        ] {
            let err = gov
                .approve_change_for_principal(&record.id, "actor-x", Some(variant), None)
                .unwrap_err();
            assert!(
                err.contains("already approved by this approver"),
                "Invisible-char variant {variant:?} should dedupe: got {err}"
            );
        }
    }

    #[test]
    fn approver_with_bom_prefix_dedupe() {
        let gov = test_gov();
        let record = gov
            .propose_change("Rule BOM", "BOM coverage", None, None, None, None)
            .unwrap();

        gov.approve_change_for_principal(&record.id, "actor-1", Some("alice"), None)
            .unwrap();

        let err = gov
            .approve_change_for_principal(&record.id, "actor-2", Some("\u{FEFF}alice"), None)
            .unwrap_err();
        assert!(
            err.contains("already approved by this approver"),
            "BOM-prefixed alice should dedupe: got {err}"
        );
    }

    #[test]
    fn expanded_invisibles_block_bidi_vs_tag_word_joiner() {
        // Each pair must dedupe: the second variant differs only by an
        // invisible codepoint (bidi, VS, tag, word joiner, soft hyphen,
        // CGJ) that previously survived the canonical key.
        let cases: &[(&str, &str)] = &[
            ("alice", "ali\u{202E}ce"),         // RLO bidi mid-string
            ("alice", "\u{2066}alice\u{2069}"), // LRI/PDI isolate pair
            ("alice", "ali\u{2060}ce"),         // word joiner
            ("alice", "ali\u{00AD}ce"),         // soft hyphen
            ("alice", "alice\u{FE0F}"),         // variation selector
            ("alice", "alice\u{E0061}"),        // tag char
            ("alice", "ali\u{034F}ce"),         // combining grapheme joiner
        ];
        for (first, second) in cases {
            let gov = test_gov();
            let record = gov
                .propose_change("Rule expanded invisibles", "Test", None, None, None, None)
                .unwrap();
            gov.approve_change_for_principal(&record.id, "actor-1", Some(first), None)
                .unwrap();
            let err = gov
                .approve_change_for_principal(&record.id, "actor-2", Some(second), None)
                .unwrap_err();
            assert!(
                err.contains("already approved by this approver"),
                "first={first:?} second={second:?} expected dedup, got: {err}"
            );
        }
    }

    #[test]
    fn invisible_only_label_rejected_after_canonicalization() {
        // Labels that strip to "" once invisibles are removed must be
        // refused — otherwise two attackers passing distinct invisible-only
        // strings would both register against an empty canonical key and
        // also match any future legitimate approver whose label happens
        // to fold to empty.
        let cases: &[&str] = &[
            "\u{200B}\u{200C}\u{200D}\u{FEFF}",
            "\u{202E}\u{202D}",
            "\u{2060}\u{00AD}\u{034F}",
            "\u{FE00}\u{E0061}",
        ];
        for label in cases {
            let gov = test_gov();
            let record = gov
                .propose_change("Rule invisible only", "Test", None, None, None, None)
                .unwrap();
            let err = gov
                .approve_change_for_principal(&record.id, "actor-1", Some(*label), None)
                .unwrap_err();
            assert!(
                err.contains("must not be empty after canonicalization"),
                "label={label:?} expected canonical-empty error, got: {err}"
            );
        }
    }

    #[test]
    fn corrupted_approvals_json_surfaces_error() {
        // Previously `serde_json::from_str(...).unwrap_or_default()`
        // silently collapsed a malformed row into an empty list and the
        // next save would erase every prior approval. The strict parse
        // must now propagate the error and roll back the txn.
        let gov = test_gov();
        let record = gov
            .propose_change("Rule corrupted json", "Test", None, None, None, None)
            .unwrap();

        gov.db
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE constitution_changes SET approvals = ?1 WHERE id = ?2",
                    rusqlite::params!["{not valid json", &record.id],
                )
                .map(|_| ())
                .map_err(|e| format!("seed corrupted approvals: {e}"))
            })
            .unwrap();

        let err = gov
            .approve_change_for_principal(&record.id, "actor-1", Some("alice"), None)
            .unwrap_err();
        assert!(
            err.contains("Approvals JSON corrupted"),
            "expected explicit corruption error, got: {err}"
        );
    }

    #[test]
    fn applied_by_metadata_escapes_via_serde() {
        // Approve through to applied state with an actor_id containing
        // characters that the previous hand-formatted JSON would have
        // mangled (`"`, `\`, newline). Read the resulting metadata_json
        // from the notes row and assert it parses cleanly as JSON with
        // the original string preserved verbatim.
        let gov = test_gov();
        let record = gov
            .propose_change("Rule injection probe", "Test", None, None, None, None)
            .unwrap();

        let evil = "alice\"\\\n; DROP TABLE notes; --";
        gov.approve_change_for_principal(&record.id, "actor-1", Some("alice"), None)
            .unwrap();
        gov.approve_change_for_principal(&record.id, "actor-2", Some("bob"), None)
            .unwrap();
        gov.approve_change_for_principal(&record.id, evil, Some("carol"), None)
            .unwrap();

        let metadata: String = gov
            .db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT metadata_json FROM notes
                     WHERE layer = 'identity_schema'
                     ORDER BY created_at DESC LIMIT 1",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|e| format!("read metadata: {e}"))
            })
            .unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&metadata).expect("metadata_json must be valid JSON");
        assert_eq!(
            parsed.get("applied_by").and_then(|v| v.as_str()),
            Some(evil),
            "applied_by must round-trip the raw evil string"
        );
    }
}
