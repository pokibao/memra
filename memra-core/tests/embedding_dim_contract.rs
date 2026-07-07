//! BUILD-4A: FastEmbed 1024-dim contract test (Rust side).
//!
//! These tests load the real fastembed BGEM3 model and assert the output
//! dimension is exactly 1024.  They are integration tests (under `tests/`)
//! so they run with `cargo test --test embedding_dim_contract`.
//!
//! Note: first run may download the ONNX model (~550 MB).  Set
//! `CARGO_TEST_TIMEOUT` or run with `--test-threads=1` to avoid OOM on CI.

use memra_core::embedding::{EMBEDDING_DIM, embed_batch, embed_text};

/// The loaded BGEM3 model must produce exactly 1024-dim vectors.
///
/// This is the primary contract gate.  If this fails it means either:
/// - fastembed-rs changed its default BGEM3 output shape, or
/// - the wrong model variant was initialised.
#[test]
fn embed_text_produces_1024_dim() {
    let vec = embed_text("memory anchor embedding dim contract probe")
        .expect("embed_text must succeed with BGEM3");
    let actual = vec.len();

    assert_eq!(
        actual, EMBEDDING_DIM,
        "embed_text produced {actual}-dim vector, expected {EMBEDDING_DIM}. \
         A fastembed model swap may have occurred.",
    );
}

/// Batch embedding must also produce 1024-dim vectors for every element.
#[test]
fn embed_batch_produces_1024_dim_for_each() {
    let texts = vec![
        "first probe text",
        "second probe text",
        "Memra stores AI memories",
    ];
    let batch = embed_batch(&texts);
    let batch_len = batch.len();
    let expected_len = texts.len();

    assert_eq!(
        batch_len, expected_len,
        "embed_batch returned {batch_len} embeddings for {expected_len} inputs",
    );

    for (i, vec) in batch.iter().enumerate() {
        let actual = vec.len();
        assert_eq!(
            actual, EMBEDDING_DIM,
            "embed_batch[{i}] produced {actual}-dim vector, expected {EMBEDDING_DIM}",
        );
    }
}

/// EMBEDDING_DIM constant must be 1024 (bge-m3 contract).
///
/// This is a compile-time + runtime check: if someone changes the constant
/// this test will catch it before it reaches production.
#[test]
fn embedding_dim_constant_is_1024() {
    assert_eq!(
        EMBEDDING_DIM, 1024,
        "EMBEDDING_DIM must be 1024 for bge-m3, got {EMBEDDING_DIM}",
    );
}

/// Two calls with the same text must produce identical vectors (determinism).
#[test]
fn embed_text_is_deterministic() {
    let text = "determinism probe";
    let v1 = embed_text(text).expect("first embed");
    let v2 = embed_text(text).expect("second embed");

    assert_eq!(v1, v2, "embed_text is not deterministic for input {text:?}",);
}
