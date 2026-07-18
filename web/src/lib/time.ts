const MINUTE = 60;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const WEEK = 7 * DAY;
// Calendar months and years vary in length; the dashboard only needs a coarse
// "a while ago" at this range, so approximate rather than pull in a date lib.
const MONTH = 30 * DAY;
const YEAR = 365 * DAY;

function plural(n: number, unit: string): string {
  return `${n} ${unit}${n === 1 ? "" : "s"} ago`;
}

/**
 * Human relative time for an RFC 3339 timestamp, e.g. "2 hours ago".
 *
 * Timestamps in the near future are reported as "just now" rather than as a
 * negative age: the browser clock and the server clock are independent, and a
 * page saved a moment ago can legitimately carry an `updated_at` a second or
 * two ahead of the client. "in -1 minutes" would be a clock bug shown to the
 * user; "just now" is what they actually did.
 */
export function formatRelativeTime(iso: string, now: Date = new Date()): string {
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return "unknown";

  const seconds = Math.floor((now.getTime() - then) / 1000);
  if (seconds < MINUTE) return "just now";
  if (seconds < HOUR) return plural(Math.floor(seconds / MINUTE), "minute");
  if (seconds < DAY) return plural(Math.floor(seconds / HOUR), "hour");
  if (seconds < WEEK) return plural(Math.floor(seconds / DAY), "day");
  if (seconds < MONTH) return plural(Math.floor(seconds / WEEK), "week");
  if (seconds < YEAR) return plural(Math.floor(seconds / MONTH), "month");
  return plural(Math.floor(seconds / YEAR), "year");
}

/** Thousands separators, so 1204 reads as 1,204 in the stats panel. */
export function formatCount(n: number): string {
  return n.toLocaleString("en-US");
}
