use memra_core::storage::writer::apply_hebbian_feedback;
use rusqlite::Connection;

fn setup_conn() -> Connection {
    let conn = Connection::open_in_memory().expect("in-memory db");
    conn.execute_batch(
        "CREATE TABLE notes (
            id TEXT PRIMARY KEY,
            source TEXT,
            confirmed_at INTEGER
        );
        CREATE TABLE note_relations (
            from_note_id TEXT NOT NULL,
            to_note_id TEXT NOT NULL,
            relation_type TEXT NOT NULL,
            strength REAL NOT NULL DEFAULT 0.5,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (from_note_id, to_note_id, relation_type)
        );
        CREATE TABLE note_events (
            id INTEGER PRIMARY KEY,
            note_id TEXT,
            event_type TEXT,
            related_note_id TEXT,
            payload_json TEXT,
            created_at TEXT
        );",
    )
    .expect("schema");
    conn
}

fn insert_note(conn: &Connection, id: &str, source: &str, confirmed_at: Option<i64>) {
    conn.execute(
        "INSERT INTO notes (id, source, confirmed_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![id, source, confirmed_at],
    )
    .expect("insert note");
}

fn insert_relation(conn: &Connection, from: &str, to: &str, strength: f64) {
    conn.execute(
        "INSERT INTO note_relations (from_note_id, to_note_id, relation_type, strength)
         VALUES (?1, ?2, 'supports', ?3)",
        rusqlite::params![from, to, strength],
    )
    .expect("insert relation");
}

fn relation_strength(conn: &Connection, from: &str, to: &str) -> f64 {
    conn.query_row(
        "SELECT strength FROM note_relations WHERE from_note_id = ?1 AND to_note_id = ?2",
        rusqlite::params![from, to],
        |row| row.get(0),
    )
    .expect("relation strength")
}

#[test]
fn trust_gate_confirmed_feedback_strengthens_user_memory() {
    let conn = setup_conn();
    insert_note(&conn, "user-note", "user", None);
    insert_note(&conn, "neighbor", "user", None);
    insert_relation(&conn, "user-note", "neighbor", 0.40);

    let affected = apply_hebbian_feedback(&conn, "user-note", "confirmed").expect("hebbian");

    assert_eq!(affected, 1);
    assert!((relation_strength(&conn, "user-note", "neighbor") - 0.45).abs() < 1e-9);
}

#[test]
fn trust_gate_confirmed_feedback_does_not_strengthen_unconfirmed_ai_candidate() {
    let conn = setup_conn();
    insert_note(&conn, "ai-note", "ai", None);
    insert_note(&conn, "neighbor", "user", None);
    insert_relation(&conn, "ai-note", "neighbor", 0.40);

    let affected = apply_hebbian_feedback(&conn, "ai-note", "confirmed").expect("hebbian");

    assert_eq!(affected, 0);
    assert!((relation_strength(&conn, "ai-note", "neighbor") - 0.40).abs() < 1e-9);
}

#[test]
fn trust_gate_confirmed_feedback_strengthens_explicitly_confirmed_candidate() {
    let conn = setup_conn();
    insert_note(&conn, "ai-note", "ai", Some(1_768_800_000));
    insert_note(&conn, "neighbor", "user", None);
    insert_relation(&conn, "ai-note", "neighbor", 0.40);

    let affected = apply_hebbian_feedback(&conn, "ai-note", "confirmed").expect("hebbian");

    assert_eq!(affected, 1);
    assert!((relation_strength(&conn, "ai-note", "neighbor") - 0.45).abs() < 1e-9);
}

#[test]
fn trust_gate_corrected_feedback_still_decays_unconfirmed_candidate() {
    let conn = setup_conn();
    insert_note(&conn, "ai-note", "ai", None);
    insert_note(&conn, "neighbor", "user", None);
    insert_relation(&conn, "ai-note", "neighbor", 0.80);

    let affected = apply_hebbian_feedback(&conn, "ai-note", "corrected").expect("hebbian");

    assert_eq!(affected, 1);
    assert!((relation_strength(&conn, "ai-note", "neighbor") - 0.68).abs() < 1e-9);
}
