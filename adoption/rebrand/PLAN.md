# Rebrand: EdgeLinkd → n2link

Status: **in progress** on branch `rebrand-n2link` (from `master` at `d692e42`). Do the rename
before the first release tag so 0.4.0 ships as n2link. No tag, publish or version change without
separate approval.

Done outside the repo (2026-10-06): GitHub organisation `n2link` created; local checkout moves to
`/media/kestl/andor/github/n2link` on andorxps; crates.io token kept in the git-ignored `.env`
(`CRATESIO_API_KEY`). Nothing is published to crates.io without separate approval.

## Decisions (2026-10-06)

| Topic | Decision |
|---|---|
| Display name | `n2link` (all lower case, also at the start of a sentence) |
| Daemon | `n2linkd` |
| Crates | `n2link-app`, `n2link-core`, `n2link-web`, `n2link-macro`, `n2link-eve`, `n2link-pymod`, `n2link-wasm-guest` |
| Rust identifiers | `n2link_core` …; `EdgelinkError` → `N2linkError`, `EdgelinkEnv` → `N2linkEnv`, `EdgelinkClass` → `N2linkClass` (Rust CamelCase) |
| Environment | `N2LINK_*` |
| Config / home | `n2linkd.toml`, `n2linkd.dev.toml`, `n2linkd.prod.toml`; home `~/.n2linkd` |
| GitHub | create the `n2link` organisation (name is free) and transfer the repo there as `n2link/n2link`; it stays a fork of `oldrev/edgelinkd`; GitHub redirects old URLs |
| Old names | keep working for **one release** (0.4.x) with a deprecation warning; removed in the next minor |
| Plugin interface | rename with **no alias** (`n2link:node/v1`, `n2link.manifest`, `n2link-wasm-guest`); ABI version stays 1 |

## Name check (2026-10-06)

- crates.io: `cargo search n2link` returns nothing.
- GitHub: user/organisation `n2link` and `n2link-io` are free; only unrelated `*/n2links` repos.
- **"N2 Link" already exists** as a product name: a NetSuite–Salesforce integration SuiteApp by
  N2 Lab (Austin, TX). It is a different market (ERP integration, not edge/IoT), but it is
  integration software with a near-identical name. A trademark search (USPTO, WIPO, and the
  markets you sell in) before public launch is recommended; this note is not legal advice.
- Domain: not checked yet (`n2link.io`, `n2link.dev`, `n2link.com`).

## Inventory (tracked files, `3rd-party/` excluded)

253 files mention `edgelink` in any case: 149 `.rs`, 10 `.py`, 23 Markdown outside
`adoption/`, 36 under `adoption/`, 4 in `crates/web/client`.

| Pattern | Occurrences | Files | Group |
|---|---|---|---|
| `EdgelinkError` / `EdgelinkEnv` / `EdgelinkClass` | 892 / 9 / 6 | 99 | mechanical |
| `edgelink_core` | 154 | 35 | mechanical |
| `edgelink_macro` | 68 | 66 | mechanical |
| `edgelink-{app,core,web,macro,eve,pymod,wasm-guest}` | 180 | 36 | mechanical |
| `edgelink_pymod` (Python imports) | 20 | 9 | mechanical (not published, no alias) |
| `edgelinkd` / `EdgeLinkd` / `EdgeLink` | 212 / 156 / 28 | ~100 | user-facing text + binary |
| `EDGELINK_*` (26 distinct variables) | 122 | 40 | compat alias |
| `edgelink:node/v1` / `edgelink.manifest` | 41 / 21 | 19 / 11 | plugin interface |
| `edgelink-credentials` | 4 | 4 | on-disk format (see below) |
| `edgelink-flow-developer` (Copilot skill dir) | 7 | 3 | rename, no alias |
| `oldrev` (upstream links, badges, PayPal, bundle id) | 30 | 9 | README / packaging |

Environment variables, by role:

- **Runtime (alias needed):** `EDGELINK_HOME`, `EDGELINK_CREDENTIAL_KEY`, `EDGELINK_RUN_ENV`.
- **Build (rename; build scripts only):** `EDGELINK_BUILD_PROFILE`, `_BUILD_TARGET`,
  `_BUILD_GIT_HASH`, `_BUILD_TIME`, `_TOOLCHAIN_TRIPLE`, `_QEMU_CMD`.
- **Tests and live acceptance (rename, no alias):** `EDGELINK_AI_*`, `_OPENAI_API_KEY`,
  `_ANTHROPIC_API_KEY`, `_XAI_API_KEY`, `_MQTT_*`, `_POSTGRES_*`, `_WEBHOOK_TOKEN`,
  `_WASM_EXAMPLES`, `_MANIFEST`, `_PHASE`.

Other user-visible strings: editor theme title (`crates/web/src/models.rs`), `/health` service
name (`crates/web/src/health.rs`), client title and two `localStorage` keys
(`crates/web/client`), `[package.metadata.bundle] identifier = "com.github.oldrev.edgelink"`,
node module names `edgelink_core` (`test-once`, `console-json`).

Found while inventorying: the home directory is `.edgelinkd` in code (`src/consts.rs`,
`localfs.rs`) but `~/.edgelink` in `src/cliargs.rs` help text. The rename fixes both.

## Compatibility for 0.4.x

| Old | New | 0.4.x behaviour |
|---|---|---|
| `edgelinkd.toml` (+ `.dev`, `.prod`) | `n2linkd.*` | Read the new name; if absent, read the old one and warn once. Never read both. |
| `~/.edgelinkd` | `~/.n2linkd` | Use the new directory; if only the old one exists, use it and warn with the `mv` command. No automatic move (it holds credentials and the key). |
| `EDGELINK_HOME`, `EDGELINK_CREDENTIAL_KEY`, `EDGELINK_RUN_ENV` | `N2LINK_*` | One helper, `env_compat("HOME")`: new name first, then old with a warning. Both set and different → startup error. |
| `edgelinkd` binary | `n2linkd` | Release archives ship an `edgelinkd` shim (symlink or tiny wrapper) that prints the deprecation and execs `n2linkd`. |
| `localStorage` `edgelinkd.client.*` | `n2linkd.client.*` | Read old once, write new. |

Removed in the release after 0.4.x; a test asserts each alias still works and warns, so removal
is a deliberate change.

### Encrypted credentials: clean break

`ENVELOPE_FORMAT = "edgelink-credentials"` is stored in every encrypted sidecar and is part of the
AEAD associated data (`credential_storage.rs`: `format \0 version \0 algorithm \0 key_id`).
Decision (2026-10-06): rename it to `n2link-credentials` with **no dual read**. The owner confirmed
that every existing credential is test data; old encrypted sidecars become unreadable and are
replaced by re-entering credentials or new test values. The release notes say so. A test asserts
that an `edgelink-credentials` envelope fails loudly (no silent empty credentials).

## Plugin interface (no alias)

`edgelink:node/v1` → `n2link:node/v1`, custom section `edgelink.manifest` → `n2link.manifest`,
`edgelink-wasm-guest` → `n2link-wasm-guest`, `edgelink::wasm` log target → `n2link::wasm`. ABI
version stays 1 because nothing was released; record it as an ADR-0002 amendment. Rebuild the
three example plugins; the hostile tests and `scripts/wasm-examples.sh --e2e` must pass. A
package built with the old names is rejected (missing manifest, or `import_forbidden`), which is
the intended behaviour.

## Legal

- Keep `LICENSE` unchanged (Apache-2.0, "Copyright (C) 2023-TODAY Li Wei and contributors").
- Add `NOTICE`: n2link is derived from EdgeLinkd (https://github.com/oldrev/edgelinkd),
  Copyright Li Wei and contributors, Apache-2.0; modifications Copyright 2026 the n2link
  contributors. Apache-2.0 §4 requires keeping notices and marking changed files; the NOTICE plus
  git history covers it.
- Source headers: 11 files say `// Copyright EdgeLink contributors`. Keep them, and add n2link
  only to new files.
- README: say plainly that n2link is a fork of EdgeLinkd. Remove the upstream author's PayPal and
  aifadian donation links and images, upstream badges, and the upstream banner/logo
  (`assets/banner.jpg`, `assets/logo.png`) unless re-used with credit. A new logo is needed.
- Do not imply endorsement by the upstream author.

## Not renamed

- `adoption/` phase records are history: leave their text, add one line at the top of
  `adoption/z8adoptionplan.md` saying the project is now n2link.
- `3rd-party/node-red` (submodule).
- `LICENSE` and the upstream copyright lines.

## Steps

Each step is one commit on `rebrand-n2link`, verified before the next.

| Step | Scope | Verify |
|---|---|---|
| R0 | This plan, `NOTICE`, `.env.example`, name checks, `n2link` GitHub org, repo moved to `n2link/n2link` | — |
| R1 | Crate/package/lib names, `use` paths, `EdgelinkError` and friends, Python module name, `Cargo.lock` | fmt, clippy `--all-features`, `cargo test --workspace --features full`, rebuild pymod + pytest |
| R2 | `n2linkd` binary, config/home names, `N2LINK_*` with the compat layer and its tests, credential tag `n2link-credentials` | R1 checks + compat tests + a run against a copy of an old `~/.edgelinkd` home (old encrypted credentials must fail loudly) |
| R3 | Plugin interface rename, guest crate, examples, ADR-0002 amendment | `cargo test -p n2link-core --features nodes_wasm`, `scripts/wasm-examples.sh --e2e` |
| R4 | UI: editor theme, client title and storage keys, `/health`, logo/favicon, bundle identifier | web tests, editor smoke test |
| R5 | README (+ `README.zh-cn.md`), docs, `AGENTS.md`, scripts, CI, `dist-pack.py`, Copilot skill directory | `git grep -i edgelink` shows only the allow-list below |
| R6 | Badges and links to `n2link/n2link`, palimnex namespace, enable Actions on the repo | CI green on `n2link/n2link` |

Allow-list for the final `git grep -i edgelink`: `LICENSE`, `NOTICE`, `adoption/**`, source
copyright headers, the compat layer and its tests, the old-credential rejection test,
`CHANGELOG`, and the README's fork sentence.

## Open items

- Logo and favicon (needed for R4).
- Domain choice and registration.
- Trademark search for "n2link" given the existing "N2 Link" product.
- CI has not run on the fork (no runs for the latest master push). GitHub keeps workflows off on
  a fork until they are enabled in its Actions tab; check that first.
