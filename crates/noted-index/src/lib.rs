// Chunking and materialisation are pure-ish: text in, rows out. The server needs
// them (it rechunks an edited page) and they cost nothing to link.
pub mod chunk;
pub mod materialize;

// Extraction types + the `ExtractionProvider` trait + `StubExtractor` are pure
// logic (no db, no network) — unlike `provider`/`worker` below, this module
// needs no feature gate. The real (LLM-backed) providers arrive in Task 5.
pub mod extract;

// In-house Louvain community detection: pure algorithm, no db, no async, no
// new dependency (`rustworkx-core` ships no Louvain at any version).
pub mod louvain;

// The community worker: hot-path local reassignment, cold-path full Louvain,
// and the churn threshold between them. Depends on `louvain` + `noted-db`;
// like `extract_worker` it needs neither `embed`'s ONNX weight nor a network
// client, so it stays unconditional rather than feature-gated.
pub mod community_worker;

// The community summariser: the `SummaryProvider` trait + a deterministic
// stub. Pure logic (no db, no network), exactly like `extract` above, so no
// feature gate. A real LLM-backed summariser would be a separate, gated
// module — see this one's header for the non-negotiable it must carry.
pub mod summary;

// The summary worker: the set-difference queue over `communities` vs
// `community_summaries`, and lazy invalidation by `member_set_hash`. Needs
// `noted-db` and `summary`, neither of which drags in `embed`'s ONNX weight
// or an HTTP client, so it stays unconditional like `extract_worker`.
pub mod summary_worker;

// Bridges `extract::Extraction` (name-based, as an extractor emits it) to
// `noted_db::graph`'s id-based primitive writes. Pure db + logic, no
// network — unconditional like `extract` above.
pub mod graph_write;

// The extraction worker (poll `graph::pending_extraction` -> extract ->
// write into every referencing workspace's graph -> mark). Depends only on
// `extract` + `graph_write` + `noted-db`, none of which need `embed`'s
// ONNX/fastembed weight, so — like `extract`/`graph_write` — this stays
// unconditional rather than gated behind a feature.
pub mod extract_worker;

// Real (LLM-backed) `ExtractionProvider` implementations (Ollama/OpenAI-
// compatible over HTTP). Gated behind `extract-ollama`: pulls in an HTTP
// client for a service that may not exist in every environment (there is
// none in this one), mirroring why `embed` gates `provider`/`worker` below.
#[cfg(feature = "extract-ollama")]
pub mod extract_providers;

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
