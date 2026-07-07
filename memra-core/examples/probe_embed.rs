//! Probe the Rust embedding backend: print first 5 dims + hash for each input.
//!
//! Gate F divergence Category C5 diagnostic tool. Rust defaults to
//! `fastembed` with the BGEM3 ONNX model (see `src/embedding/mod.rs`).
//! Python defaults to Ollama `bge-m3` (gguf). Both claim the same model but
//! the underlying artifacts differ so the raw embedding vectors are NOT
//! equal. This binary generates one half of the side-by-side evidence.
//!
//! Run:
//!     cargo run -p memra-core --example probe_embed > /tmp/probe_rust.txt
//!
//! Pair with `scripts/probe_embed_python.py` then `scripts/compare_embed.py`.

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use memra_core::embedding::{EMBEDDING_DIM, EMBEDDING_MODEL_TAG, embed_text};

/// Same 10 inputs as `scripts/probe_embed_python.py` so the two outputs line
/// up row-by-row for the compare step.
const TEST_INPUTS: &[&str] = &[
    "rollback baseline good",
    "Memra is an AI memory system",
    "Memra stores AI memories",
    "今天天气很好",
    "gate-f-auto-supersede canonical memory",
    "一个简单的 Python 测试",
    "A quick brown fox jumps over the lazy dog",
    "系统设计决策：Rust v6 对齐 Python",
    "supersede cascade threshold 0.70",
    "embedding dimension 1024 BGE-M3",
];

fn l2_norm(vec: &[f32]) -> f64 {
    vec.iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt()
}

fn print_backend_load_status() {
    match TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::BGEM3).with_show_download_progress(false),
    ) {
        Ok(_) => eprintln!("# fastembed load check: OK"),
        Err(err) => eprintln!("# fastembed load check: ERR: {err:?}"),
    }
}

fn main() {
    let backend_tag = std::env::var("MA_EMBED_BACKEND").unwrap_or_else(|_| "fastembed".into());
    print_backend_load_status();
    println!("# Rust embedding probe — backend: {backend_tag}");
    println!("# EMBEDDING_DIM constant: {EMBEDDING_DIM}");
    println!("# EMBEDDING_MODEL_TAG: {EMBEDDING_MODEL_TAG}");
    println!(
        "  # | dim  | first5                                                         | L2norm    | text"
    );
    println!("{}", "-".repeat(120));

    for (i, text) in TEST_INPUTS.iter().enumerate() {
        match embed_text(text) {
            Some(vec) => {
                let first5 = vec
                    .iter()
                    .take(5)
                    .map(|x| format!("{x:+.6}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "{i:>3} | {:>4} | [{first5}] | {:.6} | {text}",
                    vec.len(),
                    l2_norm(&vec)
                );
            }
            None => println!(
                "{i:>3} | NONE | (backend unavailable)                                          |           | {text}"
            ),
        }
    }
}
