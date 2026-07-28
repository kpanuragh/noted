//! Seed a workspace with realistic notes, for exercising search and the graph.
//!
//! The notes below are deliberately NOT written to interlock. They are the
//! sort of thing one person actually keeps — some work, some learning, some
//! domestic — written independently, on their own subjects. Whatever
//! connections the graph finds it has to find on its own; a corpus engineered
//! to link would prove only that the engineering worked.
//!
//! ```text
//! SEED_WORKSPACE=<uuid> SEED_DATABASE_URL=postgres://noted:noted@localhost:5433/noted \
//!   cargo test -p noted-server --test seed_notes -- --ignored --nocapture
//! ```
use noted_crdt::NotedDoc;
use noted_db::{blocks, docs, pages};
use uuid::Uuid;

/// (title, paragraphs). Plain prose, as typed — no headings or markup, because
/// that is what most notes actually are.
const NOTES: &[(&str, &[&str])] = &[
    (
        "Connection pooling in Postgres",
        &[
            "Each Postgres connection is a separate OS process with its own memory, so the cost of holding idle connections is real. Somewhere past a few hundred the server spends more time context switching than answering queries.",
            "PgBouncer in transaction mode is the usual answer. The catch is that anything relying on session state breaks: prepared statements, advisory locks held across statements, LISTEN/NOTIFY, and SET commands that are expected to persist.",
            "Rule of thumb I keep seeing: pool size around (cores * 2) + effective spindle count. Bigger pools usually make latency worse, not better, because the queue moves from the application into the database.",
        ],
    ),
    (
        "Rust ownership - what finally made it click",
        &[
            "Ownership stopped being confusing when I read it as a rule about who is responsible for cleanup rather than a rule about who may read a value. One owner means exactly one drop.",
            "Borrowing is the compiler proving that no reference outlives the thing it points at. The lifetime annotations are not instructions; they are the constraints being written down so the checker can verify them.",
            "The place I still get caught is returning a reference derived from a local. The error message points at the lifetime, but the actual mistake is almost always that the data should have been owned or stored somewhere longer-lived.",
        ],
    ),
    (
        "Wayanad trip - rough plan",
        &[
            "Three nights, driving up from Kozhikode. The ghat road via Thamarassery has nine hairpins and is slow behind lorries, so leave early.",
            "Edakkal caves need a decent walk uphill and get crowded after ten. Chembra peak requires a permit from the forest office and they cap the numbers, so it has to be the first thing in the morning.",
            "Homestays near Vythiri are mostly in the plantations - quiet, but the roads in are unlit and rough. Book somewhere with parking rather than trusting the last kilometre.",
        ],
    ),
    (
        "Payment webhook retries - postmortem",
        &[
            "Duplicate charges on 14 accounts. The provider retried webhooks we had already processed, because our handler returned 500 after the charge succeeded but before we wrote our own record.",
            "The fix is idempotency keys stored before doing anything else, and returning 200 for an event id we have already seen. Retrying is correct behaviour on their side; assuming exactly-once delivery was our mistake.",
            "Also worth doing: the handler did the charge and the bookkeeping in one function with no transaction boundary. Splitting the external call from the local write would have made the failure recoverable instead of ambiguous.",
        ],
    ),
    (
        "Sourdough - first attempts",
        &[
            "Starter took about twelve days to become predictable. It was lively on day three and then went completely flat for a week, which apparently is normal - the early activity is bacteria, not the yeast that eventually takes over.",
            "Hydration is the variable that changed the most for me. At 70% the dough was unmanageable by hand; at 65% it was fine and the crumb was barely different.",
            "Cold retard overnight in the fridge made more difference to flavour than anything else I tried. Bulk on the counter, shape, then twelve hours cold before baking.",
        ],
    ),
    (
        "Designing Data-Intensive Applications - notes",
        &[
            "The chapter on replication is worth rereading. Leader-based replication is easy to reason about until failover, and the failure modes there - split brain, lost writes, timing out on a leader that is merely slow - are all things I have watched happen.",
            "The distinction between consistency models is clearer framed as what the client can observe. Linearizability is a guarantee about a single object over time; serializability is about transactions over many objects. They are often confused.",
            "Log-structured storage keeps coming up. Append-only writes plus periodic compaction underlies Kafka, LSM trees, and event sourcing - three things I had filed as unrelated.",
        ],
    ),
    (
        "V60 dial-in",
        &[
            "22g in, 360g out, about 3 minutes. Grind finer than I expected - my old setting was pulling under 2:30 and tasting sour, which is under-extraction rather than the roast being wrong.",
            "Bloom with twice the coffee weight in water and wait 45 seconds. Skipping it makes the pour uneven because the trapped CO2 pushes water away from the grounds.",
            "Water temperature off the boil by about 30 seconds. Straight off the boil scorches lighter roasts and the bitterness is unmistakable.",
        ],
    ),
    (
        "Kubernetes resource limits",
        &[
            "Requests affect scheduling; limits affect runtime. Setting a request too high wastes capacity across the cluster because the scheduler reserves it whether or not it is used.",
            "CPU limits throttle rather than kill, and the throttling is enforced per 100ms period. A process that is bursty can be throttled hard while showing low average CPU, which makes latency spikes very confusing to diagnose.",
            "Memory limits do kill. OOMKilled with exit code 137 means the container exceeded its limit, not that the node ran out - those are different problems with different fixes.",
        ],
    ),
    (
        "1:1 with Priya - quarter planning",
        &[
            "She wants to move off the reporting work and into something closer to the platform. Fair - she has been on reports for three quarters and it is not going anywhere new.",
            "Concern raised: reviews are taking days to turn around and it is blocking her. Worth looking at whether we have too few people who can approve changes to that service.",
            "Action for me: work out whether the migration work starting next month is a reasonable place for her to move, and talk to Arun before promising anything.",
        ],
    ),
    (
        "Docker image size",
        &[
            "Went from 1.2GB to about 90MB. Most of it was the build toolchain sitting in the final image because everything happened in one stage.",
            "Multi-stage build with a distroless runtime did the bulk of it. Copying only the compiled binary and its runtime deps rather than the whole target directory.",
            "Layer ordering matters for rebuild speed more than for size. Dependencies before source means a code change does not invalidate the dependency layer, which took the rebuild from minutes to seconds.",
        ],
    ),
    (
        "Memory leak in the worker",
        &[
            "Steady growth of about 40MB an hour, flat CPU. Restarting fixed it, which is why it went unnoticed for weeks - the deploy cadence was hiding it.",
            "It was an unbounded channel. Producers were faster than the consumer during traffic peaks and the queue never drained fully before the next peak.",
            "Fixed with a bounded channel and backpressure. The interesting part is that the bound made the actual problem visible immediately - producers started blocking, which is the honest signal that had been swallowed by the growing buffer.",
        ],
    ),
    (
        "Nandi Hills ride",
        &[
            "About 60km each way from the north of the city. The climb is the last 8km, roughly 400m of gain - steady rather than steep, but it comes after four hours in the saddle.",
            "Leave by four in the morning. The road is genuinely dangerous after seven when the lorries start, and the gate at the top closes to cyclists at certain hours.",
            "Take more water than seems necessary. There is a stall partway up but it is unreliable, and the descent is fast enough that dehydration turns into a handling problem.",
        ],
    ),
    (
        "Thinking in Systems - notes",
        &[
            "The idea I keep coming back to: the structure of a system produces its behaviour. Blaming individual actors in a system that reliably produces the same outcome misses where the leverage is.",
            "Delays in feedback loops cause oscillation. Almost every over-correction I have seen - hiring, capacity planning, inventory - is a delayed loop being driven as though the feedback were immediate.",
            "Leverage points, roughly in ascending order of power: parameters, buffer sizes, feedback loop strength, information flows, rules, goals, paradigms. Most effort goes into the weakest ones because they are the easiest to change.",
        ],
    ),
    (
        "Home network rebuild",
        &[
            "Moved the router to the middle of the flat and the dead spot in the back bedroom disappeared entirely. Should have tried that before buying anything.",
            "Put IoT devices on a separate VLAN. Half of them phone home constantly and several have firmware that has not been updated in years.",
            "Wired the two rooms that actually matter. Wifi is fine for phones but the difference on video calls from a cable is not subtle.",
        ],
    ),
    (
        "Interview loop - backend candidate",
        &[
            "Strong on data modelling. Talked through normalisation trade-offs without being dogmatic about it, and reached for indexes only after describing the access pattern.",
            "Weaker on concurrency. Knew the vocabulary but the mutex-versus-channel discussion did not go far, and the deadlock question needed a lot of prompting.",
            "Asked good questions about how we handle on-call and what happens when something breaks. That is usually a sign of someone who has actually operated a service.",
        ],
    ),
    (
        "Onam sadya - what to cook",
        &[
            "Parippu first, then sambar, then rasam - the order matters because it is served in courses onto the same leaf.",
            "Avial needs coconut and cumin ground coarse, not to a paste. The vegetables should be in batons and still have bite; overcooking turns it into mush.",
            "Payasam last. Ada pradhaman if there is time, semiya if there is not. Jaggery rather than sugar for the ada, and the coconut milk goes in off the heat or it splits.",
        ],
    ),
    (
        "Vector search - what I learned evaluating options",
        &[
            "Recall and latency trade off through the index parameters, and the defaults are usually tuned for benchmarks rather than for a small corpus. On a few thousand vectors an exact scan is often faster than an approximate index.",
            "HNSW degrades in a specific way when you filter: the graph traversal wanders through candidates that fail the filter, so a highly selective filter can make it return far fewer results than requested.",
            "Dimensionality is a schema decision, not a runtime one. Changing the embedding model means re-embedding everything, so it is worth choosing deliberately rather than defaulting.",
        ],
    ),
    (
        "Fingerpicking practice",
        &[
            "Thumb independence is the whole thing. Alternating bass on the beat while the fingers do something syncopated feels impossible until suddenly it does not.",
            "Practising slowly with a metronome is the only thing that has worked. Speed built by playing fast is speed built on top of mistakes.",
            "Travis picking patterns transfer across a lot of songs once the right hand is automatic. Learning the pattern separately from any tune made it stick faster than learning it inside a song.",
        ],
    ),
    (
        "Talk idea - showing your work",
        &[
            "The argument: retrieval systems that answer questions should show which passages they used and how they were reached, because an answer without provenance cannot be checked.",
            "Most demos skip this because it is unflattering - showing the sources makes it obvious when the retrieval was weak and the model covered for it.",
            "Structure could be: the problem with unverifiable answers, what provenance looks like in practice, and the cost of building it. Twenty minutes, no live demo, screenshots instead.",
        ],
    ),
    (
        "Standup - things I keep forgetting to raise",
        &[
            "The staging database has been out of sync with production schema for two weeks and people are testing against it.",
            "Nobody owns the alerting config. Three alerts fired last week that nobody acted on, which means they are noise and should either be fixed or deleted.",
            "We still have no runbook for the thing that broke in March. If it happens again the same person has to be woken up.",
        ],
    ),
];

#[tokio::test]
#[ignore = "operator tool: writes notes into a real workspace"]
async fn seed_realistic_notes() {
    let url = std::env::var("SEED_DATABASE_URL")
        .or_else(|_| std::env::var("TEST_DATABASE_URL"))
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = noted_db::connect(&url).await.unwrap();
    let ws: Uuid = std::env::var("SEED_WORKSPACE")
        .expect("set SEED_WORKSPACE=<uuid>")
        .parse()
        .unwrap();

    for (title, paragraphs) in NOTES {
        let page = pages::create(&pool, ws, None, title).await.unwrap();

        // Build the document the same way the editor would, then persist the
        // whole state as one update — the CRDT log is the source of truth, so
        // writing blocks directly would leave the note empty in the editor.
        let doc = NotedDoc::new();
        for p in *paragraphs {
            doc.append_paragraph_for_test(p);
        }
        docs::append(&pool, page.id, &doc.encode_full()).await.unwrap();

        // Exactly what POST /api/pages/{id}/reproject does: project to blocks,
        // then rechunk so the note is searchable and extractable.
        let projected = doc.project();
        blocks::replace_for_page(&pool, page.id, &projected).await.unwrap();
        noted_index::materialize::rechunk_page(&pool, page.id).await.unwrap();

        println!("seeded: {title} ({} paragraphs)", paragraphs.len());
    }

    println!("\n{} notes written to {ws}", NOTES.len());
}
