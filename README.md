# noted

A self-hostable, open-source workspace for notes — with a knowledge graph built from your
own writing, and the ability to ask it questions.

Most note apps give you search. `noted` also extracts the **entities** in your notes and the
**relations** between them, clusters that graph into **themes**, and answers questions
against it. It can find a note connected to your question through a chain of ideas, not just
one that repeats your keywords — and every answer shows which passages it used and *why*.

Everything runs on your own hardware. Embeddings are local by default (ONNX/CPU, no API
key). Inference is configurable per deployment.

---

## Status

Working and tested. **Not production-ready** — read the caveats.

| Area | State |
|---|---|
| Pages, tree, block editor, real-time collaboration (Yjs/CRDT) | ✅ |
| Content-addressed chunking + local embeddings | ✅ |
| Quick find, hybrid search (full-text + vector, RRF-fused), related notes | ✅ |
| Knowledge-graph extraction with provenance | ✅ |
| Communities (in-house Louvain) + summaries | ✅ |
| Local & global graph search, Ask UI | ✅ |
| Workspace dashboard | ✅ |
| Authentication (sessions, argon2id) | ✅ |
| Multi-workspace membership + tenancy enforcement | ✅ |
| Per-page permissions, enforced on every retrieval surface | ✅ |
| Background indexing + graph reaper | ✅ |
| Databases / table / board / calendar views | ❌ not started |
| Share links (public, tokenised, revocable) | ✅ |
| **Comments, public API, templates, plugins** | ❌ not started |

**396 Rust tests, 31 web unit tests, 13 end-to-end tests.**

### Caveats worth reading before you deploy this

- **Nothing here has ever talked to a real language model.** Providers for all three
  inference roles exist and their timeouts and prompt construction are tested, but the tests
  that would verify a real model's output parses are `#[ignore]`d because this environment has
  none. Run them with `NOTED_OLLAMA_URL=... cargo test -p noted-index --features extract-ollama
  --test ollama_live -- --ignored` before trusting the graph or the answers.
- **Answer synthesis is a stub by default.** Retrieval — which passages, which entities,
  which themes, in what order — is real and fully tested. The prose that wraps it comes from
  a deterministic stub unless you configure a real model. Answer *quality* is unmeasured.
- **Entity extraction defaults to a stub too.** The graph pipeline is proven end to end, but
  a graph built by `StubExtractor` is structurally uniform and not meaningful. Point it at a
  real model to get a real graph.
- **Global search selects themes by size, not by meaning.** Ranking summaries semantically
  needs a third embedding space that does not exist yet, so a question about a niche topic
  maps over your *largest* themes, which may not include it.

---

## Quick start

Requires Docker, Rust (edition 2024), and Node 20+.

Everything runs in Docker — you need nothing else installed.

```bash
git clone git@github.com:kpanuragh/noted.git
cd noted
docker compose up -d --build
```

Open <http://localhost:3000> and create an account. The first build compiles the
Rust workspace and takes a few minutes; the embedding model (~400MB) downloads
on first start into a named volume, so it survives rebuilds.

### Or run it from source

```bash
cp .env.example .env
docker compose up -d postgres          # just the database
cargo run -p noted-server              # API on :8787
cd web && npm install && npm run dev   # web on :3000
```

### Making search and the graph work

The server indexes in the background, so writing a page is enough to make it searchable. The
CLI below is for backfilling an existing corpus, or for running extraction without turning it
on in the server:

```bash
# Embeddings only. First run downloads ~400MB of ONNX weights into .fastembed_cache/
cargo run -p noted-index --bin noted-index --features embed

# The full chain: chunk → embed → extract entities → cluster → summarise.
# Both stubs are deterministic and NOT real models — they exist so the pipeline
# is exercisable without an LLM. Each prints a loud warning saying so.
NOTED_EXTRACT=stub NOTED_SUMMARISE=stub \
  cargo run -p noted-index --bin noted-index --features embed
```

Then visit `/ask`. Local search ("about a thing") follows the graph outward from what your
question matches. Global search ("across everything") map-reduces over theme summaries.

---

## How it works

```
  Tiptap editor ──► Yjs CRDT ──► doc_updates (append-only log)
                                        │
                                        ▼
                                  blocks projection
                                        │
                          content-addressed chunking
                                        │
                     ┌──────────────────┴──────────────────┐
                     ▼                                     ▼
               embeddings                            entity extraction
            (pgvector HNSW)                        (entities + edges,
                     │                              with provenance)
                     │                                     │
                     ▼                                     ▼
          hybrid search (FTS+vector,              Louvain communities
              RRF-fused)                          + lazy summaries
                     │                                     │
                     └──────────────┬──────────────────────┘
                                    ▼
                        local & global graph search
```

**One database.** Postgres holds documents, full-text indexes, vectors (pgvector), and the
graph. Both retrieval arms and any future permission predicate run in a single query — a
split store would mean two round trips and application-side fusion.

**Work queues are queries, not tables.** Every pipeline stage is a `LEFT JOIN ... WHERE NULL`
set difference. There is no status column, no claim/lease state, and crash-safety needs no
bookkeeping: kill the indexer at any point and re-running picks up exactly the remainder.

**Content addressing.** Chunks are keyed by the SHA-256 of their text, so identical text is
stored and embedded once. Embeddings are keyed `(content_hash, model_id)` so two models can
coexist during a migration.

**Provenance everywhere.** Every graph edge records the chunk it was extracted from, which
makes re-extraction a scoped delete instead of a global recompute — and lets a citation say
which passage supports a claim.

### Crates

| Crate | Responsibility |
|---|---|
| `noted-db` | Schema, migrations, and every query. Primitives only — never depends on `noted-index`. |
| `noted-crdt` | `yrs` document handling, the update log, and plain-text extraction. |
| `noted-index` | Chunking, embeddings, extraction, clustering, summaries, graph search. Providers are traits. |
| `noted-server` | axum HTTP + the Yjs sync WebSocket. |
| `web/` | Next.js 16 / React 19 / Tiptap 3 frontend. |

### API

```
GET  /health
GET  /api/pages?workspace_id=&parent_id=      POST /api/pages
GET  /api/pages/recent?workspace_id=&limit=
GET  /api/pages/{id}                          PATCH /api/pages/{id}
POST /api/pages/{id}/reproject
GET  /api/pages/{id}/related
GET  /api/quickfind?workspace_id=&q=
GET  /api/search?workspace_id=&q=
GET  /api/ask/local?workspace_id=&q=
GET  /api/ask/global?workspace_id=&q=
GET  /api/workspaces/{workspace_id}/stats
GET  /api/workspaces/{workspace_id}/indexing
POST /api/pages/{id}/share                    DELETE /api/shares/{token}
GET  /api/shared/{token}                      (public — no session)
WS   /sync/{page_id}
```

---

## Configuring a real model

The stubs exist so the pipeline is testable without inference. Real providers sit behind
feature flags — see `crates/noted-index/src/extract_providers.rs` for the Ollama extractor
and its `NOTED_EXTRACT` gating.

If you write a provider, **give the HTTP client an explicit request and connect timeout.**
`reqwest` sets neither by default, so a hung model stalls the worker forever and no
consecutive-failure cap ever trips, because no batch ever returns.

---

## Development

```bash
cargo test --workspace -- --test-threads=1   # needs Postgres up
cargo check --workspace --all-targets

cd web
npm test              # vitest
npx tsc --noEmit
npx playwright test   # some specs need the API on :8787
```

Tests run against a real Postgres, not a mock. Scope every fixture to its own workspace —
the suite shares a database, and an instance-wide assertion will pass until someone else's
fixture makes it fail.

A few conventions this codebase holds to, learned the hard way:

- **EXPLAIN is the only proof an index is used.** "The query looks indexable" has been wrong
  here three times.
- **A test that cannot fail is not a test.** For each property test, delete the mechanism it
  protects and confirm the test dies. Several tests here passed while their subject was
  entirely absent.
- **For a negative test, where you rig the failure *is* the test.** Rigging it upstream of
  the mechanism makes the test pass by early return, and it will look correct forever.
- **One definition of "live".** Several tenancy bugs came from two queries disagreeing about
  whether a page counted. Shared definitions are spliced from one place, never re-typed.

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) — particularly the testing discipline, which is the
part of this codebase most likely to surprise you.

## License

[AGPL-3.0-or-later](LICENSE).
