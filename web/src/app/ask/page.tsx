"use client";

import { useState } from "react";
import Link from "next/link";
import {
  api,
  type Citation,
  type GlobalAnswer,
  type LocalAnswer,
} from "@/lib/api";
import styles from "@/components/dashboard.module.css";

const WORKSPACE_ID = process.env.NEXT_PUBLIC_WORKSPACE_ID ?? "";

type Mode = "local" | "global";

/**
 * Why a passage is in the answer, in the user's words.
 *
 * The whole point of the citation surface: a graph-reached passage looks
 * identical to a keyword match unless the UI says otherwise, and "why do you
 * believe this" is the question a graph-backed answer has to be able to survive.
 */
function whyLabel(why: Citation["why"]): string {
  if (why.kind === "seed") return "matched your words";
  return why.hops === 1 ? "one step away in your notes" : `${why.hops} steps away in your notes`;
}

export default function AskPage() {
  const [mode, setMode] = useState<Mode>("local");
  const [question, setQuestion] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [local, setLocal] = useState<LocalAnswer | null>(null);
  const [global_, setGlobal] = useState<GlobalAnswer | null>(null);

  async function ask(e: React.FormEvent) {
    e.preventDefault();
    const q = question.trim();
    if (!q || busy) return;

    setBusy(true);
    setError(null);
    setLocal(null);
    setGlobal(null);
    try {
      if (mode === "local") {
        setLocal(await api.askLocal(WORKSPACE_ID, q));
      } else {
        setGlobal(await api.askGlobal(WORKSPACE_ID, q));
      }
    } catch {
      setError(
        "Couldn't answer that just now. The API may be unreachable — your notes are unaffected.",
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className={styles.main}>
      <header className={styles.panel}>
        <h1 className={styles.title}>Ask your notes</h1>
        <p className={styles.subtitle}>
          <strong>About a thing</strong> follows the connections between your notes.{" "}
          <strong>Across everything</strong> reads the themes it has found. Every answer shows its
          sources.
        </p>
        <p className={styles.subtitle}>
          <Link href="/">← Back to your workspace</Link>
        </p>
      </header>

      <form onSubmit={ask} className={styles.panel} style={{ marginBottom: 24 }}>
        <div role="radiogroup" aria-label="Kind of question" className={styles.actions}>
          <button
            type="button"
            role="radio"
            aria-checked={mode === "local"}
            className={mode === "local" ? styles.primaryAction : styles.secondaryAction}
            onClick={() => setMode("local")}
          >
            About a thing
          </button>
          <button
            type="button"
            role="radio"
            aria-checked={mode === "global"}
            className={mode === "global" ? styles.primaryAction : styles.secondaryAction}
            onClick={() => setMode("global")}
          >
            Across everything
          </button>
        </div>

        <label htmlFor="question" className={styles.panelTitle} style={{ marginTop: 16 }}>
          Your question
        </label>
        <input
          id="question"
          value={question}
          onChange={(e) => setQuestion(e.target.value)}
          placeholder={
            mode === "local"
              ? "what went wrong with the Helios migration?"
              : "what have I been thinking about lately?"
          }
          style={{ width: "100%", padding: "10px 12px", marginTop: 8, borderRadius: 8, border: "1px solid rgba(128,128,128,0.35)", background: "transparent", color: "inherit", font: "inherit" }}
        />
        <div className={styles.actions} style={{ marginTop: 12 }}>
          <button type="submit" className={styles.primaryAction} disabled={busy || !question.trim()}>
            {busy ? "Reading your notes…" : "Ask"}
          </button>
        </div>
      </form>

      {error && (
        <p role="alert" className={styles.error}>
          {error}
        </p>
      )}

      {local && (
        <section className={styles.panel} aria-labelledby="answer-heading">
          <h2 id="answer-heading" className={styles.panelTitle}>
            Answer
          </h2>
          <p>{local.answer}</p>

          {local.seed_entities.length > 0 && (
            <p className={styles.subtitle}>
              Followed from:{" "}
              {local.seed_entities.map((e) => e.name).join(", ")}
            </p>
          )}

          <h3 className={styles.panelTitle} style={{ marginTop: 20 }}>
            {local.citations.length === 0
              ? "No sources"
              : `Sources (${local.citations.length})`}
          </h3>
          {local.citations.length === 0 ? (
            <p className={styles.empty}>
              Nothing in this workspace bears on that yet. Write a note, then run the indexer.
            </p>
          ) : (
            <ul className={styles.list}>
              {local.citations.map((c) => (
                <li key={c.content_hash} className={styles.listItem}>
                  <Link href={`/pages/${c.page_id}`} className={styles.pageLink}>
                    {c.title}
                  </Link>
                  <span className={styles.pageTime}>{whyLabel(c.why)}</span>
                  <p className={styles.muted}>{c.snippet}</p>
                </li>
              ))}
            </ul>
          )}
        </section>
      )}

      {global_ && (
        <section className={styles.panel} aria-labelledby="global-heading">
          <h2 id="global-heading" className={styles.panelTitle}>
            Answer
          </h2>
          <p>{global_.answer}</p>

          {/*
            Coverage is stated, never implied. An answer drawn from 3 of 40
            themes is a different claim from one drawn from all 40, and only the
            reader can decide what to do with that.
          */}
          {global_.skipped_unsummarised > 0 && (
            <p role="note" className={styles.subtitle}>
              Read {global_.partials.length} theme
              {global_.partials.length === 1 ? "" : "s"}; {global_.skipped_unsummarised} more
              {global_.skipped_unsummarised === 1 ? " has" : " have"} not been summarised yet and
              were not consulted.
            </p>
          )}

          <h3 className={styles.panelTitle} style={{ marginTop: 20 }}>
            {global_.partials.length === 0 ? "No themes" : "Themes consulted"}
          </h3>
          {global_.partials.length === 0 ? (
            <p className={styles.empty}>
              No themes have been found yet. They appear once the indexer has clustered your notes.
            </p>
          ) : (
            <ul className={styles.list}>
              {global_.partials.map((p) => (
                <li key={p.community_id} className={styles.listItem}>
                  <span className={styles.pageLink}>
                    {p.member_count} related note{p.member_count === 1 ? "" : "s"}
                  </span>
                  <span className={styles.pageTime}>
                    bearing {(p.relevance * 100).toFixed(0)}%
                    {p.was_stale ? " · summary is catching up" : ""}
                  </span>
                  <p className={styles.muted}>{p.text}</p>
                </li>
              ))}
            </ul>
          )}
        </section>
      )}
    </main>
  );
}
