"use client";

import { useState } from "react";
import { useRouter } from "next/navigation";
import { api } from "@/lib/api";
import s from "@/components/ui.module.css";

type Mode = "signin" | "signup";

/** Mirrors the server's floor in `routes/auth.rs`. The client check is a
 *  courtesy so the user learns before a round trip; the server's is what
 *  actually enforces anything. */
const MIN_PASSWORD = 12;

export default function SignInPage() {
  const router = useRouter();
  const [mode, setMode] = useState<Mode>("signin");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const tooShort = mode === "signup" && password.length > 0 && password.length < MIN_PASSWORD;

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      if (mode === "signin") await api.signIn(email.trim(), password);
      else await api.signUp(email.trim(), password);
      router.push("/");
    } catch {
      // One message for every credential failure. The server refuses to
      // distinguish "no such account" from "wrong password"; saying more here
      // would hand back the account-enumeration oracle it just closed.
      setError(
        mode === "signin"
          ? "That email and password don't match an account."
          : "Couldn't create that account. The email may already be registered.",
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className={s.centred}>
      <div style={{ width: "100%", maxWidth: 380 }}>
        <div style={{ marginBottom: 26 }}>
          <p className={s.eyebrow} style={{ marginBottom: 10 }}>
            Self-hosted knowledge base
          </p>
          <h1 style={{ marginBottom: 8 }}>
            {mode === "signin" ? "Sign in to noted" : "Create your account"}
          </h1>
          <p className={s.lede}>
            Your notes, and the knowledge graph built from them, stay on your own server.
          </p>

          {/*
            The product's thesis, in the app's own vocabulary.
            These are the exact glyphs an answer uses to show how it reached a
            passage, so the front door previews the one thing noted does that a
            search box cannot — and a returning user already knows how to read
            them by the time they see them in an answer.
          */}
          <ul className={s.list} style={{ marginTop: 22 }} aria-label="How noted answers">
            <li className={s.trace} style={{ marginBottom: 6 }}>
              <span className={s.traceGlyph}>●</span> the note that matched your words
            </li>
            <li className={s.traceDerived}>
              <span className={s.traceGlyph}>◆─</span> and the one it is connected to
            </li>
          </ul>
        </div>

        <form onSubmit={submit} className={`${s.card} ${s.cardLift}`}>
          <label className={s.label} htmlFor="email">
            Email
          </label>
          <input
            id="email"
            type="email"
            autoComplete="email"
            required
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            className={s.field}
          />

          <label className={s.label} htmlFor="password" style={{ marginTop: 16 }}>
            Password
          </label>
          <input
            id="password"
            type="password"
            autoComplete={mode === "signin" ? "current-password" : "new-password"}
            required
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            className={s.field}
          />
          {mode === "signup" && (
            <p className={s.muted} style={{ marginTop: 7, fontSize: "0.8125rem" }}>
              {tooShort
                ? `${MIN_PASSWORD} characters minimum — length beats punctuation.`
                : `At least ${MIN_PASSWORD} characters.`}
            </p>
          )}

          {error && (
            <p role="alert" className={s.error} style={{ marginTop: 16 }}>
              {error}
            </p>
          )}

          <div className={s.actions} style={{ marginTop: 20 }}>
            <button
              type="submit"
              className={s.button}
              disabled={busy || !email || !password || tooShort}
            >
              {busy ? "…" : mode === "signin" ? "Sign in" : "Create account"}
            </button>
            <button
              type="button"
              className={s.buttonQuiet}
              onClick={() => {
                setMode(mode === "signin" ? "signup" : "signin");
                setError(null);
              }}
            >
              {mode === "signin" ? "Create an account" : "I have an account"}
            </button>
          </div>
        </form>
      </div>
    </main>
  );
}
