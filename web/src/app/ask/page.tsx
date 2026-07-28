"use client";

import { useState } from "react";
import Link from "next/link";
import { api, type Citation, type GlobalAnswer, type LocalAnswer } from "@/lib/api";
import { useWorkspace } from "@/lib/useWorkspace";
import { Sidebar } from "@/components/Sidebar";
import s from "@/components/ui.module.css";

/** Style carrying a sequence position (and optionally a hop count) into CSS. */
function step(i: number, hops?: number) {
  return { "--i": i, ...(hops === undefined ? {} : { "--hops": hops }) } as React.CSSProperties;
}

/**
 * Group citations by the page they came from, preserving rank order.
 *
 * Citations are per PASSAGE, not per page — a note is split into chunks and
 * each chunk is cited on its own, which is the honest unit for an answer: you
 * want to know which paragraph supported a claim, not merely which file. But
 * rendered flat, a four-chunk note appears as four identical rows titled the
 * same thing, which reads as a duplicate bug rather than as four pieces of
 * evidence.
 *
 * Grouping keeps the precision and drops the illusion: the note is named once,
 * and every passage it contributed sits under it with its own provenance.
 */
function byPage(citations: Citation[]) {
  const groups: { pageId: string; title: string; passages: Citation[] }[] = [];
  for (const c of citations) {
    const existing = groups.find((g) => g.pageId === c.page_id);
    if (existing) existing.passages.push(c);
    else groups.push({ pageId: c.page_id, title: c.title, passages: [c] });
  }
  return groups;
}

type Mode = "local" | "global";

/**
 * THE SIGNATURE ELEMENT.
 *
 * Draws how far a passage was from the question. A direct keyword match is a
 * single neutral dot; anything the graph walked to is warm and carries one
 * diamond per hop:
 *
 *     ●        matched your words
 *     ◆─       one step away
 *     ◆─◆      two steps away
 *
 * This is information, not ornament — no other note app can draw it, because no
 * other note app knows the distance. It is also the only place the accent
 * colour is allowed on this page.
 */
function Trace({ why }: { why: Citation["why"] }) {
  if (why.kind === "seed") {
    return (
      <span className={s.trace}>
        <span className={s.traceGlyph}>●</span> matched your words
      </span>
    );
  }
  const glyph = why.hops <= 1 ? "◆─" : "◆─".repeat(Math.min(why.hops, 3));
  return (
    <span className={s.traceDerived}>
      <span className={s.traceGlyph}>{glyph}</span>
      {why.hops === 1 ? "one step away" : `${why.hops} steps away`}
    </span>
  );
}

export default function AskPage() {
  const ws = useWorkspace();
  const workspaceId = ws.status === "ready" ? ws.current : "";
  const [mode, setMode] = useState<Mode>("local");
  const [question, setQuestion] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [local, setLocal] = useState<LocalAnswer | null>(null);
  const [global_, setGlobal] = useState<GlobalAnswer | null>(null);

  async function ask(e: React.FormEvent) {
    e.preventDefault();
    const q = question.trim();
    if (!q || busy || !workspaceId) return;
    setBusy(true);
    setError(null);
    setLocal(null);
    setGlobal(null);
    try {
      if (mode === "local") setLocal(await api.askLocal(workspaceId, q));
      else setGlobal(await api.askGlobal(workspaceId, q));
    } catch {
      setError("Couldn't answer that just now. Your notes are unaffected.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className={s.app}>
      <Sidebar workspaceId={workspaceId} />
      <main className={s.main} style={{ maxWidth: 780 }}>
      <header className={s.enter} style={{ ...step(0), marginBottom: 24 }}>
        <Link href="/" className={s.backLink}>
          ← All notes
        </Link>
        <h1 style={{ marginBottom: 8 }}>Ask your notes</h1>
        <p className={s.lede}>
          Every answer shows which passages it used, and whether it found them by
          your words or by following the connections between your notes.
        </p>
      </header>

      <form
        onSubmit={ask}
        className={`${s.card} ${s.cardLift} ${s.enter}`}
        style={{ ...step(1), marginBottom: 28 }}
      >
        <div role="radiogroup" aria-label="Kind of question" className={s.segmented}>
          <button
            type="button"
            role="radio"
            aria-checked={mode === "local"}
            className={mode === "local" ? s.segItemActive : s.segItem}
            onClick={() => setMode("local")}
          >
            About a thing
          </button>
          <button
            type="button"
            role="radio"
            aria-checked={mode === "global"}
            className={mode === "global" ? s.segItemActive : s.segItem}
            onClick={() => setMode("global")}
          >
            Across everything
          </button>
        </div>

        <label htmlFor="question" className={s.label} style={{ marginTop: 18 }}>
          Your question
        </label>
        <input
          id="question"
          value={question}
          onChange={(e) => setQuestion(e.target.value)}
          className={s.field}
          placeholder={
            mode === "local"
              ? "what went wrong with the Helios migration?"
              : "what have I been thinking about lately?"
          }
        />
        <div className={s.actions} style={{ marginTop: 14 }}>
          <button type="submit" className={s.button} disabled={busy || !question.trim()}>
            {busy ? "Reading your notes…" : "Ask"}
          </button>
        </div>
      </form>

      {error && (
        <p role="alert" className={s.error}>
          {error}
        </p>
      )}

      {/* Reading notes takes a moment on a local model. Show the SHAPE of the
          answer that is coming rather than a spinner, so the wait reads as
          progress toward something. */}
      {busy && (
        <section className={s.card} aria-hidden="true">
          <div className={s.skeletonRow}>
            <div className={s.skeleton} style={{ width: "38%" }} />
            <div className={s.skeleton} style={{ width: "92%" }} />
            <div className={s.skeleton} style={{ width: "78%" }} />
          </div>
          <div className={s.skeletonRow} style={{ borderBottom: "none" }}>
            <div className={s.skeleton} style={{ width: "30%" }} />
            <div className={s.skeleton} style={{ width: "64%" }} />
          </div>
        </section>
      )}

      {local && (
        <section className={s.card} aria-labelledby="answer-heading">
          <h2 id="answer-heading" className={s.sectionTitle} style={{ marginBottom: 10 }}>
            Answer
          </h2>
          <p style={{ marginBottom: 18 }}>{local.answer}</p>

          {local.seed_entities.length > 0 && (
            // Capped. This is what the question turned out to be ABOUT, and a
            // handful of subjects is orienting; the full list from a weak
            // extractor is a paragraph of noise that buries the answer above it.
            <p className={s.muted} style={{ marginBottom: 20 }}>
              Followed from{" "}
              {local.seed_entities.slice(0, 6).map((e) => e.name).join(", ")}
              {local.seed_entities.length > 6 &&
                ` and ${local.seed_entities.length - 6} more`}
            </p>
          )}

          <hr className={s.divider} style={{ margin: "18px 0" }} />

          {/* Says WHAT was counted. "4 sources" from one note looks like a
              mistake; "4 passages from 1 note" is the same fact, understood. */}
          <h3 className={s.eyebrow} style={{ marginBottom: 8 }}>
            {local.citations.length === 0
              ? "No sources"
              : (() => {
                  const notes = byPage(local.citations).length;
                  const p = local.citations.length;
                  return `${p} passage${p === 1 ? "" : "s"} from ${notes} note${notes === 1 ? "" : "s"}`;
                })()}
          </h3>
          {local.citations.length === 0 ? (
            <p className={s.empty}>
              Nothing in this workspace bears on that yet. Write a note — it becomes
              searchable on its own within a minute or so.
            </p>
          ) : (
            <ul className={s.list}>
              {byPage(local.citations).map((g, i) => {
                // A passage found BY YOUR WORDS is already where it belongs and
                // rises in place. One the graph reached enters along the
                // connection, one beat later per hop — the same distance the
                // amber label states, said again in motion. The group takes the
                // closest passage's distance, since that is how far the answer
                // had to travel to reach this note at all.
                const nearest = g.passages.reduce(
                  (best, p) => {
                    const h = p.why.kind === "seed" ? 0 : p.why.hops;
                    return h < best ? h : best;
                  },
                  Infinity as number,
                );
                const derived = nearest > 0;
                return (
                  <li
                    key={g.pageId}
                    className={derived ? s.citeRowDerived : s.citeRow}
                    style={{ ...step(i, nearest), padding: "14px 0", borderBottom: "1px solid var(--line)" }}
                  >
                    <div style={{ display: "flex", justifyContent: "space-between", gap: 14, alignItems: "baseline" }}>
                      <Link href={`/pages/${g.pageId}`} className={s.rowTitle}>
                        {g.title}
                      </Link>
                      {/* One passage: its provenance belongs on this line.
                          Several: they may differ, so each states its own
                          below rather than one standing for all. */}
                      {g.passages.length === 1 ? (
                        <Trace why={g.passages[0].why} />
                      ) : (
                        <span className={s.rowMeta}>{g.passages.length} passages</span>
                      )}
                    </div>
                    {g.passages.map((c) => (
                      <div key={c.content_hash} className={g.passages.length > 1 ? s.passage : undefined}>
                        <p className={s.muted} style={{ marginTop: 6 }}>{c.snippet}</p>
                        {g.passages.length > 1 && (
                          <div style={{ marginTop: 4 }}>
                            <Trace why={c.why} />
                          </div>
                        )}
                      </div>
                    ))}
                  </li>
                );
              })}
            </ul>
          )}
        </section>
      )}

      {global_ && (
        <section className={s.card} aria-labelledby="global-heading">
          <h2 id="global-heading" className={s.sectionTitle} style={{ marginBottom: 10 }}>
            Answer
          </h2>
          <p style={{ marginBottom: 14 }}>{global_.answer}</p>

          {/* Coverage is stated, never implied: an answer from 3 of 40 themes is
              a different claim from one drawn from all 40. */}
          {global_.skipped_unsummarised > 0 && (
            <p role="note" className={s.muted}>
              Read {global_.partials.length} theme
              {global_.partials.length === 1 ? "" : "s"}; {global_.skipped_unsummarised} more
              {global_.skipped_unsummarised === 1 ? " has" : " have"} not been summarised yet.
            </p>
          )}

          <hr className={s.divider} style={{ margin: "18px 0" }} />

          <h3 className={s.eyebrow} style={{ marginBottom: 8 }}>
            {global_.partials.length === 0 ? "No themes" : "Themes consulted"}
          </h3>
          {global_.partials.length === 0 ? (
            <p className={s.empty}>
              No themes yet. They appear once the indexer has clustered your notes.
            </p>
          ) : (
            <ul className={s.list}>
              {global_.partials.map((p, i) => (
                <li
                  key={p.community_id}
                  className={s.citeRowDerived}
                  style={{ ...step(i, 1), padding: "12px 0", borderBottom: "1px solid var(--line)" }}
                >
                  <div style={{ display: "flex", justifyContent: "space-between", gap: 14, alignItems: "baseline" }}>
                    <span className={s.rowTitle}>
                      {p.member_count} related note{p.member_count === 1 ? "" : "s"}
                    </span>
                    <span className={s.traceDerived}>
                      <span className={s.traceGlyph}>◆</span>
                      {(p.relevance * 100).toFixed(0)}% bearing
                      {p.was_stale ? " · catching up" : ""}
                    </span>
                  </div>
                  <p className={s.muted} style={{ marginTop: 5 }}>{p.text}</p>
                </li>
              ))}
            </ul>
          )}
        </section>
      )}
      </main>
    </div>
  );
}
