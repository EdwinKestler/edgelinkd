import { useEffect, useState } from "react";

const STORAGE_KEY = "edgelinkd.client.page";
const WATCH_KEY = "edgelinkd.client.watch";
const MAX_ROWS = 12;
const MAX_FLOWS = 16;
const POLL_MS = 2000;

type PageState = {
  items: { id: string; label: string; done: boolean }[];
  level: number;
};

type ContextCell = {
  msg?: unknown;
  format?: unknown;
  forced?: unknown;
  updatedAt?: unknown;
};

type ContextRow = {
  id: string;
  scope: string;
  key: string;
  msg: string;
  forced: boolean;
  overrun: boolean | null;
  age: string | null;
};

type ContextPanel = {
  rows: ContextRow[];
  hidden: number;
  notes: string[];
};

const INITIAL: PageState = {
  items: [
    { id: "review", label: "Review the flow before deploy", done: false },
    { id: "name", label: "Name the tab after what it does", done: false },
    { id: "debug", label: "Leave a debug node on the path you care about", done: true },
  ],
  level: 40,
};

function loadState(): PageState {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) {
      return INITIAL;
    }
    const parsed = JSON.parse(raw) as PageState;
    if (!Array.isArray(parsed.items) || typeof parsed.level !== "number") {
      return INITIAL;
    }
    return parsed;
  } catch {
    return INITIAL;
  }
}

function loadWatch(): string {
  try {
    return localStorage.getItem(WATCH_KEY) ?? "scan";
  } catch {
    return "scan";
  }
}

function watchNames(text: string): string[] {
  const names = text
    .split(",")
    .map((name) => name.trim())
    .filter((name) => name.length > 0);
  return names.length === 0 ? ["scan"] : names;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function cellOf(value: unknown): ContextCell | undefined {
  if (!isRecord(value) || !("format" in value)) {
    return undefined;
  }
  return value;
}

function textOf(value: unknown): string {
  if (typeof value === "string") {
    return value;
  }
  if (value == null) {
    return "(undefined)";
  }
  return String(value);
}

function overrunOf(key: string, cell: ContextCell | undefined): boolean | null {
  if (key !== "scan" || cell?.format !== "Object" || typeof cell.msg !== "string") {
    return null;
  }
  try {
    const parsed = JSON.parse(cell.msg) as unknown;
    if (isRecord(parsed) && typeof parsed.overrun === "boolean") {
      return parsed.overrun;
    }
  } catch {
    return null;
  }
  return null;
}

function ageText(updatedAt: unknown): string | null {
  if (typeof updatedAt !== "number") {
    return null;
  }
  const seconds = Math.max(0, Math.floor((Date.now() - updatedAt) / 1000));
  if (seconds < 60) {
    return `${seconds}s ago`;
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    return `${minutes}m ago`;
  }
  return `${Math.floor(minutes / 60)}h ago`;
}

function rowsFromStores(scope: string, body: unknown, watch: string[]): ContextRow[] {
  const found = new Map<string, ContextCell>();
  if (isRecord(body)) {
    for (const store of Object.values(body)) {
      if (!isRecord(store)) {
        continue;
      }
      for (const [key, value] of Object.entries(store)) {
        const cell = cellOf(value);
        if (cell) {
          found.set(key, cell);
        }
      }
    }
  }

  const wanted = new Set(watch);
  for (const [key, cell] of found) {
    if (cell.forced === true) {
      wanted.add(key);
    }
  }

  return [...wanted].map((key) => {
    const cell = found.get(key);
    return {
      id: `${scope}:${key}`,
      scope,
      key,
      msg: textOf(cell?.msg),
      forced: cell?.forced === true,
      overrun: overrunOf(key, cell),
      age: ageText(cell?.updatedAt),
    };
  });
}

async function readBody(path: string): Promise<{ status: number; body: unknown } | "down"> {
  try {
    const response = await fetch(path);
    const body = (await response.json().catch(() => null)) as unknown;
    return { status: response.status, body };
  } catch {
    return "down";
  }
}

async function loadContext(watchText: string): Promise<ContextPanel> {
  const watch = watchNames(watchText);
  const global = await readBody("/context/global");
  if (global === "down" || global.status === 503) {
    return { rows: [], hidden: 0, notes: ["The runtime did not answer."] };
  }
  if (global.status === 401) {
    return { rows: [], hidden: 0, notes: ["The admin API requires a sign-in."] };
  }
  if (global.status !== 200) {
    return { rows: [], hidden: 0, notes: ["The runtime did not answer."] };
  }

  const rows = rowsFromStores("global", global.body, watch);
  const notes: string[] = [];
  const flows = await readBody("/flows");
  if (flows !== "down" && flows.status === 200 && isRecord(flows.body) && Array.isArray(flows.body.flows)) {
    const tabs = flows.body.flows.filter((entry) => isRecord(entry) && entry.type === "tab" && typeof entry.id === "string");
    const shown = tabs.slice(0, MAX_FLOWS);
    if (tabs.length > shown.length) {
      notes.push(`Showing the first ${MAX_FLOWS} flows.`);
    }
    for (const tab of shown) {
      if (!isRecord(tab) || typeof tab.id !== "string") {
        continue;
      }
      const label = typeof tab.label === "string" && tab.label.trim() ? tab.label : tab.id;
      const flow = await readBody(`/context/flow/${encodeURIComponent(tab.id)}`);
      if (flow === "down" || flow.status === 503) {
        return { rows: [], hidden: 0, notes: ["The runtime did not answer."] };
      }
      if (flow.status === 404) {
        notes.push(`Flow ${label} was not found.`);
        continue;
      }
      if (flow.status === 401) {
        return { rows: [], hidden: 0, notes: ["The admin API requires a sign-in."] };
      }
      if (flow.status !== 200) {
        return { rows: [], hidden: 0, notes: ["The runtime did not answer."] };
      }
      rows.push(...rowsFromStores(label, flow.body, watch));
    }
  } else {
    notes.push("The runtime did not answer.");
  }

  rows.sort((left, right) => {
    const rank = (row: ContextRow) => (row.forced ? 0 : row.key === "scan" ? 1 : 2);
    return rank(left) - rank(right) || left.scope.localeCompare(right.scope) || left.key.localeCompare(right.key);
  });
  return { rows: rows.slice(0, MAX_ROWS), hidden: Math.max(0, rows.length - MAX_ROWS), notes };
}

export function App() {
  const [state, setState] = useState<PageState>(loadState);
  const [watch, setWatch] = useState(loadWatch);
  const [panel, setPanel] = useState<ContextPanel>({ rows: [], hidden: 0, notes: [] });

  useEffect(() => {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
  }, [state]);

  useEffect(() => {
    localStorage.setItem(WATCH_KEY, watch);
  }, [watch]);

  useEffect(() => {
    let stopped = false;
    const run = () => {
      void loadContext(watch).then((next) => {
        if (!stopped) {
          setPanel(next);
        }
      });
    };
    run();
    const timer = window.setInterval(run, POLL_MS);
    return () => {
      stopped = true;
      window.clearInterval(timer);
    };
  }, [watch]);

  const done = state.items.filter((item) => item.done).length;
  const checklist = state.items.length === 0 ? 0 : Math.round((done / state.items.length) * 100);
  const progress = Math.round((checklist + state.level) / 2);

  return (
    <main className="mx-auto flex min-h-screen max-w-3xl flex-col px-6 py-16">
      <p className="text-xs tracking-[0.28em] text-mark uppercase">EdgeLinkd</p>
      <h1 className="mt-4 font-serif text-5xl leading-none font-normal text-ink">Client page</h1>
      <p className="mt-6 max-w-xl text-lg leading-relaxed text-muted">
        This page reads context from the runtime. The checklist stays in this browser. The flow editor stays at the site
        root.
      </p>

      <section className="mt-14 border-t border-line pt-8">
        <div className="flex items-baseline justify-between gap-6">
          <h2 className="font-serif text-2xl">Context</h2>
          <p className="text-sm text-muted">Read only</p>
        </div>
        <label className="mt-6 block text-sm text-muted" htmlFor="context-keys">
          Keys
          <input
            id="context-keys"
            value={watch}
            onChange={(event) => setWatch(event.target.value)}
            className="mt-2 w-full border border-line bg-raise px-3 py-2 text-ink"
            spellCheck={false}
          />
        </label>
        <p className="mt-3 text-sm text-muted">
          Comma-separated names. <span className="text-ink">scan</span> is included until you name something else. A
          forced key is shown as well. The list stays in this browser.
        </p>
        {panel.notes.map((note) => (
          <p key={note} className="mt-4 text-sm text-mark">
            {note}
          </p>
        ))}
        {panel.rows.length === 0 && panel.notes.length === 0 ? (
          <p className="mt-6 text-sm text-muted">No watched keys.</p>
        ) : panel.rows.length === 0 ? null : (
          <ul className="mt-6 divide-y divide-line">
            {panel.rows.map((row) => (
              <li key={row.id} className="flex items-start justify-between gap-6 py-4">
                <div className="min-w-0">
                  <p className="text-ink">
                    {row.scope} · {row.key}
                  </p>
                  <p className="mt-1 break-all text-sm text-muted">{row.msg}</p>
                </div>
                <div className="shrink-0 text-right text-sm">
                  {row.age ? <p className="text-muted">{row.age}</p> : null}
                  {row.forced ? <p className="text-mark">forced</p> : null}
                  {row.overrun === true ? <p className="text-mark">overrun</p> : null}
                  {row.overrun === false ? <p className="text-muted">within period</p> : null}
                </div>
              </li>
            ))}
          </ul>
        )}
        {panel.hidden > 0 ? <p className="mt-4 text-sm text-muted">Showing {MAX_ROWS} keys.</p> : null}
      </section>

      <section className="mt-12 border-t border-line pt-8">
        <div className="flex items-baseline justify-between gap-6">
          <h2 className="font-serif text-2xl">Checklist</h2>
          <p className="text-sm text-muted">
            {done} of {state.items.length}
          </p>
        </div>
        <ul className="mt-6 divide-y divide-line">
          {state.items.map((item) => (
            <li key={item.id}>
              <label className="flex cursor-pointer items-center gap-4 py-4">
                <input
                  type="checkbox"
                  checked={item.done}
                  onChange={() =>
                    setState((current) => ({
                      ...current,
                      items: current.items.map((entry) =>
                        entry.id === item.id ? { ...entry, done: !entry.done } : entry,
                      ),
                    }))
                  }
                  className="size-4 accent-mark"
                />
                <span className={item.done ? "text-muted line-through" : "text-ink"}>{item.label}</span>
              </label>
            </li>
          ))}
        </ul>
      </section>

      <section className="mt-12 border-t border-line pt-8">
        <div className="flex items-baseline justify-between gap-6">
          <h2 className="font-serif text-2xl">Level</h2>
          <p className="font-serif text-3xl text-mark">{state.level}</p>
        </div>
        <input
          type="range"
          min={0}
          max={100}
          value={state.level}
          onChange={(event) => setState((current) => ({ ...current, level: Number(event.target.value) }))}
          className="mt-6 w-full accent-mark"
          aria-label="Level"
        />
      </section>

      <section className="mt-12 border-t border-line pt-8">
        <div className="flex items-baseline justify-between gap-6">
          <h2 className="font-serif text-2xl">Progress</h2>
          <p className="text-sm text-muted">{progress}%</p>
        </div>
        <div className="mt-6 h-1.5 bg-raise" role="meter" aria-valuenow={progress} aria-valuemin={0} aria-valuemax={100}>
          <div className="h-full bg-mark" style={{ width: `${progress}%` }} />
        </div>
        <p className="mt-4 text-sm text-muted">The bar is the average of the checklist and the level. Both stay on this machine.</p>
      </section>

      <footer className="mt-auto pt-16 text-sm text-muted">
        <a className="text-ink underline decoration-line underline-offset-4" href="/">
          Open the flow editor
        </a>
      </footer>
    </main>
  );
}
