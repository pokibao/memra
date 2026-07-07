//! Probe dedup variance for Gate F category C2.
//!
//! Emits a JSON object on stdout:
//!   { "inputs": [...], "vectors": [[...], [...], ...] }
//!
//! Pair with `scripts/probe_dedup_variance.py` which embeds the same inputs
//! through the Python path and diffs the 10×10 cosine matrices. The goal is
//! to find out whether Python and Rust cross the 0.88 dedup threshold on
//! DIFFERENT pairs (embedding-distribution drift) or the SAME pairs (logic
//! drift).
//!
//! Run:
//!     cargo run --example probe_dedup_s02 > /tmp/probe_dedup_rust.json

use memra_core::embedding::embed_text;
use serde_json::json;

/// Same contents the parity harness writes in S02 (pure_write_categories).
const S02_INPUTS: &[&str] = &[
    "Category person parity sample",
    "Category place parity sample",
    "Category event parity sample",
    "Category item parity sample",
    "Category routine parity sample",
    "Category research parity sample",
    "Category decision parity sample",
    "Category architecture parity sample",
    "Category bug parity sample",
    "Category campaign parity sample",
];

fn main() {
    let mut vectors = Vec::with_capacity(S02_INPUTS.len());
    for text in S02_INPUTS {
        match embed_text(text) {
            Some(v) => vectors.push(v),
            None => {
                eprintln!("embed_text returned None for {text:?}");
                std::process::exit(1);
            }
        }
    }

    let payload = json!({
        "inputs": S02_INPUTS,
        "vectors": vectors,
    });
    println!("{}", serde_json::to_string(&payload).expect("json"));
}
