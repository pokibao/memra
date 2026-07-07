//! # memra-core
//!
//! Memra core library — the embeddable engine with no transport dependencies.
//!
//! ## Module layout (see DESIGN_DECISIONS.md D10.3)
//!
//! - `core` — kernel, write_orchestrator, safety_filter
//! - `retrieval` — fts5, vector, merge, scoring, postprocess, assemble, room
//! - `storage` — db pool, schema, migrations, notes, checkpoints, cold storage
//! - `governance` — propose, approve, constitution, actor
//! - `experience` — experience substrate tools and artifact feedback
//! - `embedding` — fastembed wrapper, model cache
//! - `llm` — Rust LLM provider clients and response/error normalization
//! - `convo_normalizer` — cold-path chat export normalization
//! - `presentation` — context_snapshot, startup_manifest, structured_mode

pub mod convo_normalizer;
pub mod core;
pub mod embedding;
pub mod experience;
pub mod governance;
pub mod llm;
pub mod personal {
    pub const TRUSTED_MEMORY_SOURCES: &[&str] = &["user", "checkpoint"];
    pub const AI_PROVISIONAL_SOURCES: &[&str] = &[
        "ai",
        "ai_extraction",
        "ai_proposal",
        "dream_promoter",
        "external_ai",
    ];
    pub const PROJECT_ENTITY_FILTERS: &[ProjectEntityFilter] = &[];
    pub const COMMERCIAL_ENTITIES: &[&str] = &[];

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ProjectEntityFilter {
        pub aliases: &'static [&'static str],
        pub rooms: &'static [&'static str],
    }

    pub fn is_trusted_memory_source(source: &str) -> bool {
        source_matches(source, TRUSTED_MEMORY_SOURCES)
    }

    pub fn is_ai_provisional_source(source: &str) -> bool {
        source_matches(source, AI_PROVISIONAL_SOURCES)
    }

    pub fn allows_strengthening(
        source: Option<&str>,
        confirmed: bool,
        trust_missing_source: bool,
    ) -> bool {
        confirmed
            || source
                .map(is_trusted_memory_source)
                .unwrap_or(trust_missing_source)
    }

    pub fn project_entity_filters() -> &'static [ProjectEntityFilter] {
        PROJECT_ENTITY_FILTERS
    }

    pub fn commercial_entities() -> &'static [&'static str] {
        COMMERCIAL_ENTITIES
    }

    fn source_matches(source: &str, allowed: &[&str]) -> bool {
        let source = source.trim();
        allowed
            .iter()
            .any(|allowed_source| source.eq_ignore_ascii_case(allowed_source))
    }
}
pub mod retrieval;
pub mod storage;
