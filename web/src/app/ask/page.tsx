"use client";

import { useState } from "react";
import Link from "next/link";
import { api, type Citation, type GlobalAnswer, type LocalAnswer } from "@/lib/api";
import { useWorkspace } from "@/lib/useWorkspace";
import s from "@/components/ui.module.css";

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
    <main className={s.main} style={{ maxWidth: 760, margin: "0 auto" }}>
      <header style={{ marginBottom: 24 }}>
        <p className={s.eyebrow} style={{ marginBottom: 10 }}>
          <Link href="/" style={{ textDecoration: "none" }}>
            ← Workspace
          </Link>
        </p>
        <h1 style={{ marginBottom: 8 }}>Ask your notes</h1>
        <p className={s.lede}>
          Every answer shows which passages it used, and whether it found them by
          your words or by following the connections between your notes.
        </p>
      </header>

      <form onSubmit={ask} className={`${s.card} ${s.cardLift}`} style={{ marginBottom: 28 }}>
        <div role="radiogroup" aria-label="Kind of question" className={s.actions}>
          <button
            type="button"
            role="radio"
            aria-checked={mode === "local"}
            className={mode === "local" ? s.button : s.buttonQuiet}
            onClick={() => setMode("local")}
          >
            About a thing
          </button>
          <button
            type="button"
            role="radio"
            aria-checked={mode === "global"}
            className={mode === "global" ? s.button : s.buttonQuiet}
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

      {local && (
        <section className={s.card} aria-labelledby="answer-heading">
          <h2 id="answer-heading" className={s.sectionTitle} style={{ marginBottom: 10 }}>
            Answer
          </h2>
          <p style={{ marginBottom: 18 }}>{local.answer}</p>

          {local.seed_entities.length > 0 && (
            <p className={s.muted} style={{ marginBottom: 20 }}>
              Followed from {local.seed_entities.map((e) => e.name).join(", ")}
            </p>
          )}

          <hr className={s.divider} style={{ margin: "18px 0" }} />

          <h3 className={s.eyebrow} style={{ marginBottom: 8 }}>
            {local.citations.length === 0
              ? "No sources"
              : `${local.citations.length} source${local.citations.length === 1 ? "" : "s"}`}
          </h3>
          {local.citations.length === 0 ? (
            <p className={s.empty}>
              Nothing in this workspace bears on that yet. Write a note — it becomes
              searchable on its own within a minute or so.
            </p>
          ) : (
            <ul className={s.list}>
              {local.citations.map((c) => (
                <li key={c.content_hash} style={{ padding: "12px 0", borderBottom: "1px solid var(--line)" }}>
                  <div style={{ display: "flex", justifyContent: "space-between", gap: 14, alignItems: "baseline" }}>
                    <Link href={`/pages/${c.page_id}`} className={s.rowTitle}>
                      {c.title}
                    </Link>
                    <Trace why={c.why} />
                  </div>
                  <p className={s.muted} style={{ marginTop: 5 }}>{c.snippet}</p>
                </li>
              ))}
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
              {global_.partials.map((p) => (
                <li key={p.community_id} style={{ padding: "12px 0", borderBottom: "1px solid var(--line)" }}>
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
  );
}
