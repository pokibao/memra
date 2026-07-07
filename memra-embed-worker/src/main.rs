//! # memra-embed-worker
//!
//! Standalone embedding subprocess — isolates ONNX FFI crashes from the main
//! server process. Communicates via Unix domain socket (Phase 3).
//!
//! Phase 0: just verifies fastembed-rs loads and can encode a test string.

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("ma_embed_worker=info")
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("memra-embed-worker starting — loading model...");

    let mut model = TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::ParaphraseMLMiniLML12V2).with_show_download_progress(true),
    )?;

    // Quick smoke test
    let test_texts = vec!["Memra embedding test"];
    let embeddings = model.embed(test_texts, None)?;

    tracing::info!(
        "Model loaded OK. Test embedding dim = {}",
        embeddings.first().map_or(0, |e| e.len()),
    );

    // Phase 0: just exit after smoke. Phase 3 adds UDS server loop.
    Ok(())
}
