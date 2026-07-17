// Chunking and materialisation are pure-ish: text in, rows out. The server needs
// them (it rechunks an edited page) and they cost nothing to link.
pub mod chunk;
pub mod materialize;

// Embedding drags in fastembed -> ONNX runtime + a HuggingFace model downloader.
// Only the `noted-index` binary embeds anything, so this is default-off: the web
// server has no business linking an inference runtime it never calls.
#[cfg(feature = "embed")]
pub mod provider;
#[cfg(feature = "embed")]
pub mod worker;
