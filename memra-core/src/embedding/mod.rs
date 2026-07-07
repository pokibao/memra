//! Embedding service: thread-safe singleton fastembed wrapper.
//!
//! Model: `BGEM3` (1024-dim) — matches Python prod `bge-m3` / Ollama default.
//! Route A (default): fastembed-rs native BGEM3, zero network dependency.
//! Route B (MA_EMBED_BACKEND=ollama): HTTP client → Ollama localhost:11434 (not yet implemented).

use std::sync::{Mutex, OnceLock};

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use tracing::{info, warn};

/// Output dimension for BGEM3 (bge-m3).
pub const EMBEDDING_DIM: usize = 1024;

/// Human-readable model tag (matches Python prod model name).
pub const EMBEDDING_MODEL_TAG: &str = "bge-m3";

/// Expected byte size of a serialized embedding blob (EMBEDDING_DIM * 4 bytes per f32).
pub const EMBEDDING_BLOB_BYTES: usize = EMBEDDING_DIM * 4; // 4096 bytes

/// Global singleton embedding model (Mutex for `&mut self` on `embed`).
static EMBED_MODEL: OnceLock<Option<Mutex<TextEmbedding>>> = OnceLock::new();

/// Initialize (or get) the global embedding model.
///
/// Returns `None` if model loading failed.
fn get_model() -> Option<&'static Mutex<TextEmbedding>> {
    EMBED_MODEL
        .get_or_init(|| {
            match TextEmbedding::try_new(
                InitOptions::new(EmbeddingModel::BGEM3).with_show_download_progress(false),
            ) {
                Ok(model) => {
                    info!("Loaded fastembed model: BGEM3 ({EMBEDDING_DIM}-dim)");
                    Some(Mutex::new(model))
                }
                Err(e) => {
                    warn!("Failed to load fastembed model: {e}");
                    None
                }
            }
        })
        .as_ref()
}

/// Embed a single text. Returns `None` if model is unavailable or dim mismatch.
pub fn embed_text(text: &str) -> Option<Vec<f32>> {
    let mutex = get_model()?;
    let mut model = mutex.lock().ok()?;
    match model.embed(vec![text], None) {
        Ok(mut embeddings) => {
            if embeddings.len() == 1 {
                let vec = embeddings.remove(0);
                if vec.len() != EMBEDDING_DIM {
                    warn!(
                        "embed_text: dim mismatch — got {} expected {}",
                        vec.len(),
                        EMBEDDING_DIM
                    );
                    return None;
                }
                Some(vec)
            } else {
                warn!("embed_text: unexpected {} embeddings", embeddings.len());
                None
            }
        }
        Err(e) => {
            warn!("embed_text failed: {e}");
            None
        }
    }
}

/// Embed a batch of texts. Returns empty vec if model is unavailable.
/// Each embedding is checked for correct dim; on mismatch the whole batch is returned empty.
pub fn embed_batch(texts: &[&str]) -> Vec<Vec<f32>> {
    let Some(mutex) = get_model() else {
        return vec![];
    };
    let Ok(mut model) = mutex.lock() else {
        return vec![];
    };
    match model.embed(texts, None) {
        Ok(embeddings) => {
            // Verify every embedding has the correct dimension
            for (i, emb) in embeddings.iter().enumerate() {
                if emb.len() != EMBEDDING_DIM {
                    warn!(
                        "embed_batch: dim mismatch at index {i} — got {} expected {}",
                        emb.len(),
                        EMBEDDING_DIM
                    );
                    return vec![];
                }
            }
            embeddings
        }
        Err(e) => {
            warn!("embed_batch failed: {e}");
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_returns_correct_dim() {
        let vec = embed_text("Hello world").expect("should embed");
        assert_eq!(vec.len(), EMBEDDING_DIM);
    }

    #[test]
    fn embed_singleton_is_consistent() {
        let v1 = embed_text("test").expect("should embed");
        let v2 = embed_text("test").expect("should embed");
        // Same input → same output (deterministic ONNX)
        assert_eq!(v1, v2);
    }

    #[test]
    fn embed_batch_works() {
        let texts = vec!["hello", "world"];
        let result = embed_batch(&texts);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), EMBEDDING_DIM);
    }

    #[test]
    fn embed_cosine_similar_texts() {
        let v1 = embed_text("Memra is an AI memory system").expect("embed");
        let v2 = embed_text("Memra stores AI memories").expect("embed");
        let v3 = embed_text("今天天气很好").expect("embed");

        let sim_12 = crate::retrieval::scoring::cosine_similarity(&v1, &v2);
        let sim_13 = crate::retrieval::scoring::cosine_similarity(&v1, &v3);

        // Similar texts should have higher similarity
        assert!(
            sim_12 > sim_13,
            "Similar texts should be closer: sim_12={sim_12:.4}, sim_13={sim_13:.4}"
        );
        assert!(
            sim_12 > 0.7,
            "Related texts should have > 0.7 cosine: {sim_12:.4}"
        );
    }
}
