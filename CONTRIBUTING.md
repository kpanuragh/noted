# Contributing to noted

Thanks for looking. This document is short on ceremony and long on the two or three habits
that actually keep this codebase correct.

## Getting set up

```bash
cp .env.example .env
docker compose up -d          # Postgres 17 + pgvector on :5433
cargo test --workspace -- --test-threads=1
cd web && npm install && npm test
```

Tests run against a **real Postgres**, not a mock. If you see `PoolTimedOut`, check
`docker compose ps` before you debug anything else — a stopped container has been
misdiagnosed as an application deadlock here more than once.

## The testing discipline

Most of the defects this project has shipped were not wrong code. They were **tests that
could not fail**. Six separate tests passed while the thing they were named for was absent
or unreachable. So:

**1. Delete the mechanism, confirm the test dies.**
For every property test, name the mechanism it protects, remove that mechanism, and check the
test actually fails. If it still passes, the test is measuring something else — however
plausible its name. Routing a test through production code is necessary and *not sufficient*:
one convergence property here ran through the real worker and still survived deleting the
entire hot path it existed to check.

**2. For a negative test, where you rig the failure *is* the test.**
"Prove X rolls back when the transaction fails" is only proven if the failure happens *after*
X. Rig it upstream and the test passes by early return, and it will look correct forever. For
"prove this is inside the transaction", a `DEFERRABLE INITIALLY DEFERRED` constraint trigger
puts the failure at COMMIT, which is exactly where you need it.

**3. Assert the premise your fixture depends on.**
If a test means "this page is reachable only through the graph", assert that plain search
does *not* return it — in the same test. An unasserted claim about a fixture is a comment,
not a test.

**4. EXPLAIN is the only proof an index is used.**
"The query looks indexable" has been wrong three times here. Prove it with a real
counterfactual — drop the index inside a rolled-back transaction and compare plans.
`plan.contains("my_index")` cannot distinguish *chosen* from *mentioned*.

**5. Count with `==`, never `<=`.**
Especially for "this did no work" assertions. Guard every zero-count assertion with a
preceding one proving the code ran at all, or it passes because nothing happened.

**6. Leave no debris.**
The suite shares a database. Scope every fixture to its own workspace, and never assert
anything instance-wide — a `LIMIT`-bounded assertion over the whole instance will pass until
someone else's fixture makes it fail. One test once left 754,906 rows behind.

## Architecture rules

- **`noted-db` must never depend on `noted-index`.** The dependency runs one way. `noted-db`
  deals in primitives (`Uuid`, `String`, tuples), never in domain types from the layer above.
- **One definition of "live".** Four separate data-loss bugs came from two queries disagreeing
  about whether a page counted. Shared definitions are spliced from a single place
  (`clusterable_edges_cte!`), never re-typed. Do not add a competing definition.
- **Ask what a key gates.** A globally shared, content-addressed key is safe when it gates a
  *per-content* artifact (an embedding is a pure function of text and model). It is a bug when
  it gates a *per-tenant* artifact — one row can never represent N tenants. Three data-loss
  bugs here were exactly this mistake.
- **Work queues are queries, not tables.** Set differences (`LEFT JOIN ... WHERE NULL`), no
  status column, no claim/lease state. Crash-safety falls out for free.

## Commits and pull requests

- Branch from `main`: `feat/<issue>-<slug>` or `fix/<issue>-<slug>`.
- Sign your commits (`git config commit.gpgsign true`). All history here is signed.
- Subject line says what changed and why it matters, in the imperative. Body optional.
- Open a PR referencing its issue (`Closes #N`). Say what you verified, not just what you
  wrote — paste the failing-then-passing output for anything you claim to have fixed.
- `cargo test --workspace -- --test-threads=1`, `cargo check --workspace --all-targets`, and
  the web suite must all be green.

## Reporting a real problem is a success

If a task turns out to be wrong, unbuildable, or to require weakening a test to pass, **say
so and stop**. That is more valuable than a green suite. Several changes here were correctly
blocked by someone noticing the brief itself was wrong, and each time they were right.
