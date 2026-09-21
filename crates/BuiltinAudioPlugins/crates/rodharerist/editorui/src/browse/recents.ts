/** Session-only recents. Frontend-owned; not plugin/DSP state. */

export type RecentKind = "preset" | "nam" | "ir" | "tone3000";

export type RecentItem = {
  kind: RecentKind;
  id: string;
  title: string;
  source: string;
  /** Local filename when the item can be re-read from disk. */
  fileName?: string;
  at: number;
};

const KEY = "rodhareist.recents";
const MAX = 24;

function read(): RecentItem[] {
  try {
    const raw = sessionStorage.getItem(KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw) as unknown;
    return Array.isArray(parsed) ? (parsed as RecentItem[]) : [];
  } catch {
    return [];
  }
}

function write(items: RecentItem[]): void {
  try {
    sessionStorage.setItem(KEY, JSON.stringify(items.slice(0, MAX)));
  } catch {
    /* private mode */
  }
}

export function listRecents(): RecentItem[] {
  return read();
}

export function rememberRecent(item: Omit<RecentItem, "at">): RecentItem[] {
  const next = [
    { ...item, at: Date.now() },
    ...read().filter((entry) => !(entry.kind === item.kind && entry.id === item.id)),
  ].slice(0, MAX);
  write(next);
  return next;
}
