"use client";

/**
 * Last-resort net for a render error that escapes the per-panel boundaries —
 * one thrown by the page's own shell rather than by a panel.
 *
 * Deliberately NOT the primary defence. Next.js replaces the entire route's
 * content with this component, so relying on it alone would turn one bad panel
 * into a blank dashboard, which is the failure being fixed. PanelBoundary
 * contains a panel's crash to that panel; this only runs when there is no
 * smaller blast radius left, and it still offers recovery rather than a dead
 * end.
 */
export default function Error({ reset }: { error: Error; reset: () => void }) {
  return (
    <main style={{ padding: 32, maxWidth: 640 }}>
      <h1 style={{ fontSize: 20, marginBottom: 8 }}>Something went wrong</h1>
      <p role="alert" style={{ marginBottom: 16 }}>
        The dashboard couldn&apos;t be displayed. Your notes are safe and
        unchanged — this is a display problem, not a data problem.
      </p>
      <button type="button" onClick={reset}>
        Try again
      </button>
    </main>
  );
}
