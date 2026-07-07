//! SQLite storage layer — connection, schema, row types, write ops, cold storage.

pub mod cold_storage;
pub mod db;
pub mod session_tokens_writer;
pub mod sessions_writer;
pub mod writer;
