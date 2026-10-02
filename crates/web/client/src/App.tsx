import { useEffect, useState } from "react";

const STORAGE_KEY = "edgelinkd.client.page";

type PageState = {
  items: { id: string; label: string; done: boolean }[];
  level: number;
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

export function App() {
  const [state, setState] = useState<PageState>(loadState);

  useEffect(() => {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
  }, [state]);

  const done = state.items.filter((item) => item.done).length;
  const checklist = state.items.length === 0 ? 0 : Math.round((done / state.items.length) * 100);
  const progress = Math.round((checklist + state.level) / 2);

  return (
    <main className="mx-auto flex min-h-screen max-w-3xl flex-col px-6 py-16">
      <p className="text-xs tracking-[0.28em] text-mark uppercase">EdgeLinkd</p>
      <h1 className="mt-4 font-serif text-5xl leading-none font-normal text-ink">Client page</h1>
      <p className="mt-6 max-w-xl text-lg leading-relaxed text-muted">
        This page runs in the browser. Nothing here is sent to the runtime. The flow editor stays at the site root.
      </p>

      <section className="mt-14 border-t border-line pt-8">
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
