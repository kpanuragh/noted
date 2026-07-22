"use client";

import { useState } from "react";
import { useRouter } from "next/navigation";
import { api } from "@/lib/api";
import styles from "@/components/dashboard.module.css";

type Mode = "signin" | "signup";

/** Mirrors the server's floor (`routes/auth.rs`). Kept in sync deliberately:
 *  the client check is a courtesy so the user learns before a round trip, and
 *  the server's is the one that actually enforces anything. */
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
      if (mode === "signin") {
        await api.signIn(email.trim(), password);
      } else {
        await api.signUp(email.trim(), password);
      }
      router.push("/");
    } catch {
      // Deliberately one message for every credential failure. The server
      // refuses to distinguish "no such account" from "wrong password" — saying
      // so here would hand back the account-enumeration oracle it just closed.
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
    <main className={styles.main} style={{ maxWidth: 460 }}>
      <div className={styles.panel}>
        <h1 className={styles.title}>{mode === "signin" ? "Sign in to noted" : "Create an account"}</h1>
        <p className={styles.subtitle}>
          Your notes, and the knowledge graph built from them, stay on your own server.
        </p>

        <form onSubmit={submit}>
          <label htmlFor="email" className={styles.panelTitle} style={{ marginTop: 16 }}>
            Email
          </label>
          <input
            id="email"
            type="email"
            autoComplete="email"
            required
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            style={field}
          />

          <label htmlFor="password" className={styles.panelTitle} style={{ marginTop: 16 }}>
            Password
          </label>
          <input
            id="password"
            type="password"
            autoComplete={mode === "signin" ? "current-password" : "new-password"}
            required
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            style={field}
          />
          {mode === "signup" && (
            <p className={styles.subtitle} style={{ marginTop: 6 }}>
              {tooShort
                ? `At least ${MIN_PASSWORD} characters — length beats punctuation.`
                : `At least ${MIN_PASSWORD} characters.`}
            </p>
          )}

          {error && (
            <p role="alert" className={styles.error} style={{ marginTop: 12 }}>
              {error}
            </p>
          )}

          <div className={styles.actions} style={{ marginTop: 16 }}>
            <button
              type="submit"
              className={styles.primaryAction}
              disabled={busy || !email || !password || tooShort}
            >
              {busy ? "…" : mode === "signin" ? "Sign in" : "Create account"}
            </button>
            <button
              type="button"
              className={styles.secondaryAction}
              onClick={() => {
                setMode(mode === "signin" ? "signup" : "signin");
                setError(null);
              }}
            >
              {mode === "signin" ? "Create an account" : "I already have one"}
            </button>
          </div>
        </form>
      </div>
    </main>
  );
}

const field: React.CSSProperties = {
  width: "100%",
  padding: "10px 12px",
  marginTop: 8,
  borderRadius: 8,
  border: "1px solid rgba(128,128,128,0.35)",
  background: "transparent",
  color: "inherit",
  font: "inherit",
};
