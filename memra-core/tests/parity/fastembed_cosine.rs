//! Phase 0 GO/NO-GO gate: FastEmbed vector parity test.
//!
//! Loads archived v6 reference embeddings kept as Rust-owned fixture data under
//! `memra-core/tests/fixtures/embed_parity_reference.json` and compares against
//! Rust fastembed-rs output using the same model (BGEM3, 1024-dim).
//!
//! Gate: cosine similarity > 0.98 for all 20 samples.
//!
//! Threshold rationale (T1 packet R2): both sides use BAAI/bge-m3 weights, but
//! Rust runs ONNX fp32 while Python/Ollama runs GGUF quantized. Different runtimes
//! introduce small numerical differences — 0.9999 was over-strict; 0.98 is the
//! engineering-reasonable lower bound for "same model, different runtime" parity.

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
struct ParityReference {
    model: String,
    sample_count: usize,
    dimension: usize,
    samples: Vec<ParitySample>,
}

#[derive(Deserialize)]
struct ParitySample {
    text: String,
    vector: Vec<f64>,
}

fn cosine_similarity(a: &[f32], b: &[f64]) -> f64 {
    assert_eq!(a.len(), b.len(), "vector dimensions must match");
    let mut dot: f64 = 0.0;
    let mut norm_a: f64 = 0.0;
    let mut norm_b: f64 = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        let xf = *x as f64;
        dot += xf * y;
        norm_a += xf * xf;
        norm_b += y * y;
    }
    if norm_a <= 0.0 || norm_b <= 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

fn l2_distance(a: &[f32], b: &[f64]) -> f64 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = *x as f64 - y;
            d * d
        })
        .sum::<f64>()
        .sqrt()
}

#[test]
fn fastembed_parity_gate() {
    let ref_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/embed_parity_reference.json");

    assert!(
        ref_path.exists(),
        "FastEmbed reference fixture missing after R4 archive move.\n\
         Expected: {}",
        ref_path.display()
    );

    let data = std::fs::read_to_string(&ref_path).expect("read reference JSON");
    let reference: ParityReference = serde_json::from_str(&data).expect("parse reference JSON");

    assert_eq!(reference.sample_count, 20, "expected 20 test samples");
    assert_eq!(
        reference.dimension, 1024,
        "expected 1024-dim vectors (bge-m3)"
    );

    // Load Rust fastembed with the same model
    let mut model = TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::BGEM3).with_show_download_progress(true),
    )
    .expect("failed to load BGEM3 model");

    let texts: Vec<&str> = reference.samples.iter().map(|s| s.text.as_str()).collect();
    let rust_embeddings = model.embed(texts, None).expect("embedding failed");

    assert_eq!(rust_embeddings.len(), reference.samples.len());

    // Gate check
    let mut all_pass = true;
    let mut min_cosine = 1.0_f64;
    let mut max_l2 = 0.0_f64;
    let mut report_lines: Vec<String> = Vec::new();

    report_lines.push("# FastEmbed Parity Report".to_string());
    report_lines.push(format!("Model: {}", reference.model));
    report_lines.push(format!("Samples: {}", reference.sample_count));
    report_lines.push(format!("Dimension: {}", reference.dimension));
    report_lines.push(String::new());
    report_lines.push("| # | Cosine | L2 | Pass | Text (first 60 chars) |".to_string());
    report_lines.push("|---|--------|-----|------|----------------------|".to_string());

    for (i, (rust_vec, sample)) in rust_embeddings.iter().zip(&reference.samples).enumerate() {
        let cos = cosine_similarity(rust_vec, &sample.vector);
        let l2 = l2_distance(rust_vec, &sample.vector);
        // Gate: cosine > 0.98 is the GO/NO-GO gate (updated from 0.9999).
        // Threshold rationale: ONNX fp32 (Rust) vs GGUF quantized (Python/Ollama)
        // introduces small numerical differences; 0.98 is the engineering-reasonable
        // lower bound for "same weights, different runtime" parity (T1 packet R2).
        // L2 distance is informational only — MA uses cosine for search.
        let pass = cos > 0.98;

        if !pass {
            all_pass = false;
        }
        min_cosine = min_cosine.min(cos);
        max_l2 = max_l2.max(l2);

        let text_preview: String = sample.text.chars().take(60).collect();
        let status = if pass { "PASS" } else { "FAIL" };
        report_lines.push(format!(
            "| {} | {:.8} | {:.6} | {} | {} |",
            i + 1,
            cos,
            l2,
            status,
            text_preview
        ));
    }

    report_lines.push(String::new());
    report_lines.push(format!("Min cosine: {min_cosine:.10}"));
    report_lines.push(format!("Max L2: {max_l2:.10}"));
    report_lines.push(format!("Gate: {}", if all_pass { "PASS" } else { "FAIL" }));

    let report_path = std::env::temp_dir().join("ma-fastembed-parity-report.md");
    std::fs::write(&report_path, report_lines.join("\n")).expect("write report");
    eprintln!("Parity report written to: {}", report_path.display());

    // Assert gate
    assert!(
        all_pass,
        "FastEmbed parity FAILED. Min cosine: {min_cosine:.10}, Max L2: {max_l2:.10}. \
         See the temp parity report path printed above.",
    );
    assert!(
        min_cosine > 0.98,
        "Cosine threshold violated: {min_cosine:.10} (gate: > 0.98)"
    );
    // L2 is informational — magnitude normalization differs between
    // Python/Rust ONNX, but cosine parity is what matters for search.
    eprintln!("L2 info: max={max_l2:.6} (not gated — cosine parity is sufficient for MA search)");
}
