"use client";

import { Component, type ReactNode } from "react";
import styles from "./dashboard.module.css";

/**
 * Contains a render-time throw to the one panel that threw.
 *
 * The panels already catch *rejected* promises, but that only covers failures
 * the fetch layer reports. A value that arrives successfully and then explodes
 * during render — iterating a non-array, reading a field off null — happens
 * after the promise resolved, so no `.catch()` can see it, and React responds
 * to an uncaught render error by unmounting the whole tree. One malformed
 * payload therefore blanked the entire dashboard: exactly the outcome the
 * per-panel isolation exists to prevent.
 *
 * `src/lib/api.ts` now rejects malformed bodies at the boundary, which is the
 * real fix. This is defence in depth for the render errors that validation
 * cannot anticipate — a panel is the correct blast radius, so the failure
 * stays local and the rest of the dashboard keeps working.
 *
 * Must be a class: React exposes error boundaries only through
 * `componentDidCatch` / `getDerivedStateFromError`, with no hook equivalent.
 */
export class PanelBoundary extends Component<
  { title: string; children: ReactNode },
  { crashed: boolean }
> {
  state = { crashed: false };

  static getDerivedStateFromError() {
    return { crashed: true };
  }

  render() {
    if (this.state.crashed) {
      return (
        <section className={styles.panel} aria-label={this.props.title}>
          <h2 className={styles.panelTitle}>{this.props.title}</h2>
          <p className={styles.error} role="alert">
            This panel couldn&apos;t be displayed. The rest of your workspace is
            unaffected — reload the page to try again.
          </p>
        </section>
      );
    }
    return this.props.children;
  }
}
