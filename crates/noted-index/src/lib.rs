// Chunking and materialisation are pure-ish: text in, rows out. The server needs
// them (it rechunks an edited page) and they cost nothing to link.
pub mod chunk;
pub mod materialize;

// Embedding drags in fastembed -> ONNX runtime + a HuggingFace model downloader.
// `noted-server` DOES use this now — it embeds search queries for hybrid search
// (see noted-server's routes/search.rs) — but not every consumer of this crate
// wants that weight (e.g. a future CLI that only chunks/materializes). Keeping
// the feature default-off lets those non-search consumers stay slim while the
// server opts in explicitly.
#[cfg(feature = "embed")]
pub mod provider;
#[cfg(feature = "embed")]
pub mod worker;
