//! Canonical read helpers for resolving lineage heads.
//!
//! This mirrors the stable core of `backend/services/canonical_reader.py`:
//! resolve by canonical `root_id` first, fall back to `topic_key`, and ignore
//! inactive/non-head rows. The Rust surface hydrates full `NoteRow`s because
//! downstream search/retrieval code works with the typed row rather than
//! arbitrary column projections.

use std::collections::{BTreeSet, HashMap};

use rusqlite::{Connection, params_from_iter};

use crate::storage::db::NoteRow;

const SQLITE_CHUNK_SIZE: usize = 900;

type LineageFields = (Option<String>, Option<String>);
type SeedLineageMap = HashMap<String, LineageFields>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalReadSource {
    Canonical,
    TopicKeyFallback,
    Miss,
}

impl CanonicalReadSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Canonical => "canonical",
            Self::TopicKeyFallback => "topic_key_fallback",
            Self::Miss => "miss",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CanonicalSeed {
    pub seed_id: String,
    pub note_id: Option<String>,
    pub root_id: Option<String>,
    pub topic_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CanonicalRead {
    pub note: Option<NoteRow>,
    pub source: CanonicalReadSource,
}

#[derive(Debug, Clone)]
struct NormalizedSeed {
    seed_id: String,
    root_id: Option<String>,
    topic_key: Option<String>,
}

pub fn hydrate_head_by_canonical_or_topic(
    conn: &Connection,
    note_id: Option<&str>,
    root_id: Option<&str>,
    topic_key: Option<&str>,
) -> rusqlite::Result<CanonicalRead> {
    let resolved = hydrate_heads_by_canonical_or_topic(
        conn,
        &[CanonicalSeed {
            seed_id: "__single__".to_string(),
            note_id: normalized_opt(note_id).map(str::to_string),
            root_id: normalized_opt(root_id).map(str::to_string),
            topic_key: normalized_opt(topic_key).map(str::to_string),
        }],
    )?;
    Ok(resolved
        .get("__single__")
        .cloned()
        .unwrap_or(CanonicalRead {
            note: None,
            source: CanonicalReadSource::Miss,
        }))
}

pub fn hydrate_heads_by_canonical_or_topic(
    conn: &Connection,
    seeds: &[CanonicalSeed],
) -> rusqlite::Result<HashMap<String, CanonicalRead>> {
    let note_seed_ids = seeds
        .iter()
        .filter(|seed| seed.root_id.is_none() || seed.topic_key.is_none())
        .filter_map(|seed| seed.note_id.as_deref())
        .filter_map(normalized_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let note_seed_map = hydrate_seed_lineage(conn, &note_seed_ids)?;

    let normalized = seeds
        .iter()
        .map(|seed| {
            let mut root_id = normalized_opt(seed.root_id.as_deref()).map(str::to_string);
            let mut topic_key = normalized_opt(seed.topic_key.as_deref()).map(str::to_string);
            if let Some(note_id) = seed.note_id.as_deref().and_then(normalized_str) {
                if let Some((seed_root, seed_topic)) = note_seed_map.get(note_id) {
                    if root_id.is_none() {
                        root_id = seed_root.clone();
                    }
                    if topic_key.is_none() {
                        topic_key = seed_topic.clone();
                    }
                }
            }
            NormalizedSeed {
                seed_id: seed.seed_id.clone(),
                root_id,
                topic_key,
            }
        })
        .collect::<Vec<_>>();

    let root_ids = normalized
        .iter()
        .filter_map(|seed| seed.root_id.as_deref())
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    let topic_keys = normalized
        .iter()
        .filter_map(|seed| seed.topic_key.as_deref())
        .map(str::to_string)
        .collect::<BTreeSet<_>>();

    let canonical_by_root = hydrate_heads_by_field(conn, "root_id", &root_ids)?;
    let fallback_by_topic = hydrate_heads_by_field(conn, "topic_key", &topic_keys)?;

    let mut resolved = HashMap::new();
    for seed in normalized {
        let read = if let Some(root_id) = seed.root_id.as_deref() {
            canonical_by_root
                .get(root_id)
                .cloned()
                .map(|note| CanonicalRead {
                    note: Some(note),
                    source: CanonicalReadSource::Canonical,
                })
        } else {
            None
        }
        .or_else(|| {
            seed.topic_key.as_deref().and_then(|topic_key| {
                fallback_by_topic
                    .get(topic_key)
                    .cloned()
                    .map(|note| CanonicalRead {
                        note: Some(note),
                        source: CanonicalReadSource::TopicKeyFallback,
                    })
            })
        })
        .unwrap_or(CanonicalRead {
            note: None,
            source: CanonicalReadSource::Miss,
        });
        resolved.insert(seed.seed_id, read);
    }

    Ok(resolved)
}

fn normalized_opt(value: Option<&str>) -> Option<&str> {
    value.and_then(normalized_str)
}

fn normalized_str(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() { None } else { Some(value) }
}

fn hydrate_seed_lineage(
    conn: &Connection,
    note_ids: &[String],
) -> rusqlite::Result<SeedLineageMap> {
    let mut output = HashMap::new();
    for chunk in note_ids.chunks(SQLITE_CHUNK_SIZE) {
        if chunk.is_empty() {
            continue;
        }
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, root_id, topic_key
             FROM notes
             WHERE id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(chunk.iter()), |row| {
            Ok((
                row.get::<_, String>("id")?,
                row.get::<_, Option<String>>("root_id")?
                    .and_then(normalized_owned),
                row.get::<_, Option<String>>("topic_key")?
                    .and_then(normalized_owned),
            ))
        })?;
        for row in rows {
            let (id, root_id, topic_key) = row?;
            output.insert(id, (root_id, topic_key));
        }
    }
    Ok(output)
}

fn hydrate_heads_by_field(
    conn: &Connection,
    field: &str,
    values: &BTreeSet<String>,
) -> rusqlite::Result<HashMap<String, NoteRow>> {
    debug_assert!(matches!(field, "root_id" | "topic_key"));
    let mut output = HashMap::new();
    let values = values.iter().cloned().collect::<Vec<_>>();
    for chunk in values.chunks(SQLITE_CHUNK_SIZE) {
        if chunk.is_empty() {
            continue;
        }
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT n.*
             FROM notes n
             WHERE n.{field} IN ({placeholders})
               AND COALESCE(n.is_head, 1) = 1
               AND COALESCE(n.is_active, 1) = 1
               AND COALESCE(n.evolution_state, 'active') = 'active'
             ORDER BY n.{field} ASC, COALESCE(n.created_at, '') DESC, n.id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(chunk.iter()), NoteRow::from_row)?;
        for row in rows {
            let note = row?;
            let key = match field {
                "root_id" => note.root_id.as_deref(),
                "topic_key" => note.topic_key.as_deref(),
                _ => None,
            }
            .and_then(normalized_str)
            .map(str::to_string);
            if let Some(key) = key {
                output.entry(key).or_insert(note);
            }
        }
    }
    Ok(output)
}

fn normalized_owned(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::DbPool;

    struct NoteFixture<'a> {
        id: &'a str,
        root_id: Option<&'a str>,
        topic_key: Option<&'a str>,
        is_active: i64,
        is_head: i64,
        evolution_state: &'a str,
        created_at: &'a str,
    }

    impl<'a> NoteFixture<'a> {
        fn head(
            id: &'a str,
            root_id: Option<&'a str>,
            topic_key: Option<&'a str>,
            created_at: &'a str,
        ) -> Self {
            Self {
                id,
                root_id,
                topic_key,
                is_active: 1,
                is_head: 1,
                evolution_state: "active",
                created_at,
            }
        }

        fn superseded_tail(
            id: &'a str,
            root_id: Option<&'a str>,
            topic_key: Option<&'a str>,
            created_at: &'a str,
        ) -> Self {
            Self {
                id,
                root_id,
                topic_key,
                is_active: 1,
                is_head: 0,
                evolution_state: "superseded",
                created_at,
            }
        }
    }

    fn insert_note(conn: &Connection, note: NoteFixture<'_>) {
        conn.execute(
            "INSERT INTO notes (
                id, content, layer, project_id, is_active, is_head, evolution_state,
                root_id, topic_key, created_at, updated_at
             ) VALUES (?1, ?2, 'verified_fact', 'test', ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            rusqlite::params![
                note.id,
                format!("content-{}", note.id),
                note.is_active,
                note.is_head,
                note.evolution_state,
                note.root_id,
                note.topic_key,
                note.created_at,
            ],
        )
        .expect("insert note");
    }

    #[test]
    fn hydrate_prefers_canonical_root_over_topic_fallback() {
        let pool = DbPool::open_in_memory().expect("pool");
        pool.with_conn(|conn| {
            insert_note(
                conn,
                NoteFixture::head(
                    "canonical-head",
                    Some("root-a"),
                    Some("topic-a-v2"),
                    "2026-04-20T00:00:02Z",
                ),
            );
            insert_note(
                conn,
                NoteFixture::head(
                    "legacy-topic",
                    None,
                    Some("topic-a-v1"),
                    "2026-04-20T00:00:01Z",
                ),
            );

            let resolved =
                hydrate_head_by_canonical_or_topic(conn, None, Some("root-a"), Some("topic-a-v1"))
                    .expect("hydrate");

            assert_eq!(resolved.source, CanonicalReadSource::Canonical);
            assert_eq!(
                resolved.note.as_ref().map(|note| note.id.as_str()),
                Some("canonical-head")
            );
        });
    }

    #[test]
    fn hydrate_uses_topic_fallback_for_legacy_rows() {
        let pool = DbPool::open_in_memory().expect("pool");
        pool.with_conn(|conn| {
            insert_note(
                conn,
                NoteFixture::head(
                    "legacy-head",
                    None,
                    Some("legacy-topic"),
                    "2026-04-20T00:00:01Z",
                ),
            );

            let resolved =
                hydrate_head_by_canonical_or_topic(conn, None, None, Some("legacy-topic"))
                    .expect("hydrate");

            assert_eq!(resolved.source, CanonicalReadSource::TopicKeyFallback);
            assert_eq!(
                resolved.note.as_ref().map(|note| note.id.as_str()),
                Some("legacy-head")
            );
        });
    }

    #[test]
    fn hydrate_returns_current_head_for_superseded_tail_note_id() {
        let pool = DbPool::open_in_memory().expect("pool");
        pool.with_conn(|conn| {
            insert_note(
                conn,
                NoteFixture::superseded_tail(
                    "tail-v1",
                    Some("root-c"),
                    Some("topic-c-v1"),
                    "2026-04-20T00:00:01Z",
                ),
            );
            insert_note(
                conn,
                NoteFixture::head(
                    "head-v2",
                    Some("root-c"),
                    Some("topic-c-v2"),
                    "2026-04-20T00:00:02Z",
                ),
            );

            let resolved = hydrate_head_by_canonical_or_topic(conn, Some("tail-v1"), None, None)
                .expect("hydrate");

            assert_eq!(resolved.source, CanonicalReadSource::Canonical);
            assert_eq!(
                resolved.note.as_ref().map(|note| note.id.as_str()),
                Some("head-v2")
            );
        });
    }

    #[test]
    fn batch_hydrate_chunks_more_than_sqlite_variable_limit() {
        let pool = DbPool::open_in_memory().expect("pool");
        pool.with_conn(|conn| {
            let mut seeds = Vec::new();
            for i in 0..1200 {
                let id = format!("note-{i}");
                let root_id = format!("root-{i}");
                let topic_key = format!("topic-{i}");
                insert_note(
                    conn,
                    NoteFixture::head(
                        &id,
                        Some(&root_id),
                        Some(&topic_key),
                        "2026-04-21T00:00:00Z",
                    ),
                );
                seeds.push(CanonicalSeed {
                    seed_id: format!("seed-{i}"),
                    note_id: None,
                    root_id: Some(root_id),
                    topic_key: Some(topic_key),
                });
            }

            let resolved = hydrate_heads_by_canonical_or_topic(conn, &seeds).expect("hydrate");

            assert_eq!(resolved.len(), 1200);
            for i in 0..1200 {
                let read = resolved.get(&format!("seed-{i}")).expect("seed");
                let expected = format!("note-{i}");
                assert_eq!(read.source, CanonicalReadSource::Canonical);
                assert_eq!(
                    read.note.as_ref().map(|note| note.id.as_str()),
                    Some(expected.as_str())
                );
            }
        });
    }
}
