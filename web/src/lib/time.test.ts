import { describe, expect, it } from "vitest";
import { formatCount, formatRelativeTime } from "./time";

const NOW = new Date("2026-07-18T12:00:00.000Z");

/** `seconds` ago, as an RFC 3339 string. */
function ago(seconds: number): string {
  return new Date(NOW.getTime() - seconds * 1000).toISOString();
}

describe("formatRelativeTime", () => {
  it("says 'just now' under a minute, on both sides of the boundary", () => {
    expect(formatRelativeTime(ago(0), NOW)).toBe("just now");
    expect(formatRelativeTime(ago(59), NOW)).toBe("just now");
    expect(formatRelativeTime(ago(60), NOW)).toBe("1 minute ago");
  });

  it("singularises one unit and pluralises the rest", () => {
    expect(formatRelativeTime(ago(60), NOW)).toBe("1 minute ago");
    expect(formatRelativeTime(ago(120), NOW)).toBe("2 minutes ago");
    expect(formatRelativeTime(ago(3600), NOW)).toBe("1 hour ago");
    expect(formatRelativeTime(ago(7200), NOW)).toBe("2 hours ago");
    expect(formatRelativeTime(ago(86400), NOW)).toBe("1 day ago");
    expect(formatRelativeTime(ago(2 * 86400), NOW)).toBe("2 days ago");
  });

  it("crosses from minutes to hours to days at the right seconds", () => {
    expect(formatRelativeTime(ago(3599), NOW)).toBe("59 minutes ago");
    expect(formatRelativeTime(ago(3600), NOW)).toBe("1 hour ago");
    expect(formatRelativeTime(ago(86399), NOW)).toBe("23 hours ago");
    expect(formatRelativeTime(ago(86400), NOW)).toBe("1 day ago");
    expect(formatRelativeTime(ago(7 * 86400 - 1), NOW)).toBe("6 days ago");
    expect(formatRelativeTime(ago(7 * 86400), NOW)).toBe("1 week ago");
  });

  it("keeps going into weeks, months and years", () => {
    expect(formatRelativeTime(ago(21 * 86400), NOW)).toBe("3 weeks ago");
    expect(formatRelativeTime(ago(30 * 86400), NOW)).toBe("1 month ago");
    expect(formatRelativeTime(ago(200 * 86400), NOW)).toBe("6 months ago");
    expect(formatRelativeTime(ago(365 * 86400), NOW)).toBe("1 year ago");
    expect(formatRelativeTime(ago(800 * 86400), NOW)).toBe("2 years ago");
  });

  it("reports a future timestamp as 'just now' instead of a negative age", () => {
    // Clock skew between server and browser must not surface as "in -1 minutes".
    expect(formatRelativeTime(ago(-30), NOW)).toBe("just now");
    expect(formatRelativeTime(ago(-90000), NOW)).toBe("just now");
  });

  it("returns 'unknown' for an unparseable timestamp rather than 'NaN ... ago'", () => {
    expect(formatRelativeTime("not a date", NOW)).toBe("unknown");
    expect(formatRelativeTime("", NOW)).toBe("unknown");
  });
});

describe("formatCount", () => {
  it("groups thousands", () => {
    expect(formatCount(0)).toBe("0");
    expect(formatCount(999)).toBe("999");
    expect(formatCount(1204)).toBe("1,204");
    expect(formatCount(1234567)).toBe("1,234,567");
  });
});
