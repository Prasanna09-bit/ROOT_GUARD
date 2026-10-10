# RootGuard

**Failure intelligence for developers: root cause → fix → prevention.**

RootGuard turns a single failure into a *cited* root-cause chain, finds the
commit that introduced it, and shows every site in your codebase that shares
the failure class.

```
Failure → Evidence → Root Cause → Related Sites → Prevention
```

> **V3 scope:** deterministic only. No AI, no network, no database.
> Every claim carries a citation (`file:line` or `commit sha`); anything we
> could not observe is explicitly marked `HYPOTHESIS`, never asserted as fact.
> `explain --verify` then *checks* the analysis: reproduce the instance (T1),
> resolve every citation (T2), and mutation-check a generated regression
> guard against a known-good revision (T3). Between runs, a local **failure
> memory** recalls past sightings, decays old verdicts until revalidated, and
> records your triage so tiers can be calibrated.

---

## Install

```bash
cargo install --path .        # from this repo
# or
cargo build --release         # binary at target/release/rootguard
```

Requires: `git` on PATH. Nothing else.

---

## Commands

### 1. `rootguard explain` — paste any error, get a cited 5-why chain

```bash
cargo build 2>&1 | rootguard explain -
# or
rootguard explain "error[E0425]: unresolved name \`user_id\`
  --> src/db.rs:42:9"
# or from a file
rootguard explain build.log
```

Output:

```
RootGuard analysis
========================================================================
fingerprint : compile/user_id/src_db_rs
kind        : compile
git         : 4241a6f9 (clean)

Failure chain (5 whys)
------------------------------------------------------------------------
1. [symptom] OBSERVED
     unresolved name `user_id` [compile error] at src/db.rs:42:9
     → src/db.rs:42:9: primary location reported by the failing tool

2. [immediate-cause] OBSERVED
     the compiler/type-checker rejected `user_id` — the program never ran; …

3. [underlying-cause] OBSERVED
     the failing line was last modified in `4241a6f9` — "refactor: rewrite
     total to use add" (by Demo) — the strongest available candidate …
     → commit 4241a6f9: refactor: rewrite total to use add

4. [systemic-cause] OBSERVED
     `user_id` is referenced at 7 other sites — the same failure class can
     surface anywhere this pattern repeats
     → 7 sites: e.g. src/db.rs:12, src/api.rs:88 …

5. [prevention] HYPOTHESIS
     prevent recurrence: a compile/type check or lint rule rejecting this
     pattern at every site (cheapest: lint rule; then unit test)

Related sites in this failure class (7)
------------------------------------------------------------------------
  src/api.rs:88  [possible]
  src/db.rs:12   [likely]
  …
```

Supported parsers: **rustc / cargo**, **tsc / TypeScript**, **Python
tracebacks**, **Node/V8 stacks**, **Rust test-harness panics**, plus a
generic fallback.

#### `explain --verify` — prove it with the T1/T2/T3 ladder

```bash
rootguard explain build.log --verify --test "cargo test foo"
rootguard explain build.log --verify --good v1.2.0    # T3 counterfactual ref
```

| Stage | Question | How |
|---|---|---|
| **T1 · instance** | Does the failure still reproduce? | runs `--test` at HEAD, re-parses output, compares fingerprints |
| **T2 · citations** | Do the citations still resolve? | every `file:line` (in range) and `commit <sha>` (object exists) |
| **T3 · mutation** | Does the generated guard discriminate? | guard must **fail** at HEAD and **pass** at a known-good ref, checked in a throwaway worktree |

The verdict is a tier over what actually ran — a stage that could not run is
absent (plus a note), never reported as a pass:

* **CONFIRMED** — all three stages ran and passed.
* **LIKELY** — every stage that ran passed, but the full ladder didn't run
  (e.g. no `--test`, or no good endpoint).
* **POSSIBLE** — nothing ran, or at least one stage that ran did not verify.

```
Verification ladder
------------------------------------------------------------------------
T1 instance  : reproduced (exit 1, fingerprint match)
T2 citations : 4/4 resolved
T3 mutation  : `python3 app.py` fails at HEAD, passes at HEAD~1 — guard verified
overall tier : CONFIRMED

Guards (1)
------------------------------------------------------------------------
  python3 app.py | runtime/runtimeerror/app_py | src/app.py:3
```

Safety: `--verify` never moves your HEAD (the counterfactual runs in a
temporary worktree, removed on all paths), never reverts tracked files, and
skips auto-detecting a good endpoint on a dirty tree (pass `--good`).

#### Failure memory — every `explain` remembers

Each `explain` (and `watch`) records the sighting in
`<root>/.rootguard/failures/*.yaml` and recalls it on later runs:

```
Failure memory
------------------------------------------------------------------------
  seen 4 times since 2026-10-10 (last 2026-10-10)
  last verified 2026-08-01 (70d ago): CONFIRMED → decayed to POSSIBLE
  revalidate: re-run `rootguard explain --verify --test '<cmd>'`
  triage: 1 correct / 0 wrong
```

* **Sightings ≠ trust.** Seeing the failure again only bumps the counter;
  only a fresh `--verify` run refreshes the verification date.
* **Decay.** 30 days after a verification its tier drops one rank per further
  30 days (floor `possible`) until a ladder run revalidates it.
* **Triage.** `--triage correct|wrong` records your verdict against the
  fingerprint; `rootguard calibrate` turns those verdicts into an agreement
  report per tier.
* **Local and invisible.** Records are written under `.rootguard/`, which the
  first write adds to the repo's *local* exclude (`.git/info/exclude`) — never
  your tracked `.gitignore`, never `git status` (so T3's clean-tree checks
  keep working). Opt out entirely with `--no-memory` (`explain` and `watch`).

### 2. `rootguard watch` — run a command, analyze it automatically on failure

```bash
rootguard watch -- cargo test
rootguard watch -- pytest -x
rootguard watch -- sh -c 'npm run build'
```

* Passes the child's **exit code through** (CI-safe: `rootguard watch -- make check` fails the build).
* Streams stderr live while capturing it for analysis.
* On success: prints nothing but a one-line notice.

### 3. `rootguard bisect` — find the commit that broke it

```bash
rootguard bisect --test "cargo test foo"
rootguard bisect --test "pytest tests/test_api.py" --good v1.2.0
rootguard bisect --test "node scripts/check.js" --bad HEAD~1
```

* Auto-detects and **verifies** a good endpoint (runs your test there first —
  never trusts an unchecked `merge-base`).
* Wraps your test so probe artifacts can't abort the bisect, scrubs between
  steps, and always restores HEAD + resets the bisect session.
* Refuses to start on a dirty tree.

```
RootGuard bisect
========================================================================
test command : python3 -c "import app; app.main()"
range        : 4eb394e1..HEAD
revisions tested: 1

FIRST BAD COMMIT
  sha     : fe31c3ef0922b3444c7a4c21d7535399f25ce8ee
  author  : Demo
  summary : BUG
```

### 4. `rootguard calibrate` — are the tiers honest?

```bash
rootguard calibrate               # text
rootguard calibrate -f yaml       # machine-readable
```

Aggregates every triage verdict in the failure memory, bucketed by the tier
that was in effect when the verdict was recorded:

```
RootGuard calibration
========================================================================
records       : 4 under .rootguard/failures
triage        : 5 verdicts — 4 correct, 1 wrong (80% agreement)

agreement by tier at last verification:
  confirmed         3 correct     0 wrong
  likely            1 correct     0 wrong
  possible          0 correct     1 wrong
  never-verified    1 correct     0 wrong
```

A tier whose verdicts keep coming back `wrong` is over-promising — the report
says so plainly. Calibration *measures* the wording; it does not silently
rewrite it.

---

## Output formats

Every command accepts `-f text` (default) and `-f yaml`:

```bash
rootguard explain build.log -f yaml
```

```yaml
rootguard: 1
observed:
  source: file:build.log
  fingerprint: compile/user_id/src_db_rs
  git: {head: 4241a6f9…, branch: main, clean: true}
  exit_code: ~
normalized:
  kind: compile
  message: unresolved name `user_id`
  symbol: user_id
  location: src/db.rs:42:9
analysis:
  steps:
    - level: symptom
      text: …
      confidence: observed       # observed | hypothesis
      evidence: [{cite: "src/db.rs:42:9", note: …}]
  suspects:
    - {commit: …, author: …, summary: …, why: …, tier: confirmed}
  sites:
    - {file: src/api.rs, line: 88, tier: possible}
verification:                    # ~ unless --verify ran
  tier: confirmed                # confirmed | likely | possible
  t1_instance: {command: cargo test foo, exit_code: 101, reproduced: true, …}
  t2_citations: {checked: 4, resolved: 4, unresolved: []}
  t3_mutation: {check: cargo test foo, head_fails: true, good: v1.2.0,
                good_passes: true, mutation_checked: true}
  notes: []
guards:                          # generated by --verify
  - "cargo test foo | test/user_id/src_db_rs | src/db.rs:42:9"
memory:                          # omitted unless the failure memory ran
  first_seen: 2026-10-10T09:58:59Z
  last_seen: 2026-10-10T10:12:00Z
  observations: 4
  verified_at: 2026-08-01T09:00:00Z
  verified_tier: confirmed       # what the ladder said then
  current_tier: possible         # after decay — what is trustworthy now
  age_days: 70
  stale: true
  correct: 4
  wrong: 1
```

The YAML schema (`rootguard: 1`) is the contract for integrations — the CLI,
future skill/MCP wrappers, and CI reporting all speak it.

---

## Design rules

1. **Deterministic layer first.** Parsing, fingerprinting, blame, symbol scan
   and the chain template are pure code — no model in the loop. AI can be
   layered on later without changing this contract.
2. **Cite or mark as hypothesis.** A step with no evidence may never claim
   `observed`. Enforced by test (`chain_is_complete_and_cited`).
3. **Fingerprints are structural**, not textual: `kind/symbol/normalized-path`.
   Same failure at different line numbers or absolute paths ⇒ same fingerprint.
4. **Fail safely.** Bisect refuses dirty trees, probes both endpoints, scrubs
   probe artifacts, and restores HEAD on every exit path — including errors.
   Verification reuses the same posture: worktree-only counterfactuals, no
   tracked-file reverts, dirty trees skip auto good-detection.
5. **Noise floors.** Symbols shorter than 3 chars are excluded from class
   expansion rather than matching half the repo.
6. **Tiers earn their words.** `confirmed` is reserved for what git or the
   ladder proved (line introduction, observed failing site, mutation-checked
   guard); everything weaker is `likely` or `possible`. A ladder stage that
   could not run reports itself absent, never as a pass.
7. **Memory decays; sightings don't count.** A stored verification is evidence
   about the past: it ages out of the tiers unless a fresh ladder run
   revalidates it. Recording a sighting never upgrades trust.

---

## Fingerprinting

```
compile/user_id/src_db_rs
runtime/a/src_calc_py
type/response/src_api_users_ts
```

`kind` + normalized `symbol` (lowercased, digits stripped) + last two path
segments (absolute prefixes, digits stripped). Stable across machines and
line-number churn — this is the key that joins failure records over time in
the failure memory.

---

## What's in this repo / what's next

| V1 + V2 + V3 (this repo) | Later |
|---|---|
| `explain`, `watch`, `bisect`, `calibrate` | CI bot / skill / MCP surfaces |
| 5-why chain + citations | feedback-driven tier auto-tuning |
| fingerprinting | multi-repo failure memory sharing |
| repo-wide symbol scan | |
| class expansion w/ confidence tiers (`confirmed`/`likely`/`possible`) | |
| regression guard generation (filled `guards`) | |
| **verification ladder**: T1 instance, T2 citations, T3 mutation-check (filled `verification`) | |
| **failure memory** (`.rootguard/failures/*.yaml`): recall, decay + revalidation, triage → calibration | |

Explicitly **deferred**: AI reasoning layer, graph database, LSP integration,
multi-CI ingestion — each only when the data proves it earns its place.

---

## Development

```bash
cargo test          # 77 tests: unit + fixtures + bisect + ladder + failure memory
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

Layout:

```
src/
├── ingest/        # paste / stdin / file input, command runner
├── normalize/     # parsers (rustc, tsc, python, node, test-panic) + fingerprint
├── codeintel/     # repo-wide whole-word symbol scan
├── gitintel/      # blame, line/file history, bisect orchestration
├── reason/        # 5-why chain builder (observed vs hypothesis, tiers)
├── guard/         # regression-guard generation (kind × toolchain)
├── verify/        # T1/T2/T3 verification ladder + tier aggregation
├── memory/        # failure memory: recall, decay + revalidation, calibration
└── report/        # text + YAML renderers
fixtures/errors/   # one file per parser
tests/             # explain.rs (fixtures), bisect.rs, verify.rs, memory.rs
```

### Testing philosophy

- **Parsers**: golden fixtures per language.
- **Chain**: shape, citation rules, hypothesis honesty.
- **Fingerprint**: stable across path prefixes and re-runs.
- **Bisect**: builds throwaway git repos with a *seeded* breaking commit and
  asserts we find exactly that commit, restore HEAD, leave a clean tree, and
  refuse dirty/never-failing cases.
- **Verification**: seeds a failing repo and asserts the full ladder
  (reproduce → cite → mutate), tier demotion on every failure mode, and the
  fail-safe guarantees (HEAD/tree untouched, user files preserved).
- **Memory**: sightings accumulate, verifications decay on schedule and
  revalidate on a fresh ladder run, corrupt/foreign files are ignored,
  calibration buckets verdicts by tier — and memory stays invisible to
  `git status` without ever blocking the ladder.

---

## Honest limitations (V3)

- No AI: explanations are template-driven from deterministic evidence. The
  chain is structured and cited, but it will not invent a clever hypothesis.
- Blame requires committed lines; uncommitted/shallow history falls back to
  file history and is marked `hypothesis`.
- Parsers cover rustc/tsc/python/node + a generic fallback. Unknown formats
  still work but degrade to `parse_confident: false`.
- Class expansion is whole-word symbol search, not type-aware — it can
  over-match generic identifiers (the 3-char floor helps but doesn't
  eliminate it). Site/suspect tiers quantify exactly this doubt.
- Guard generation is heuristic (kind × toolchain): it guesses the cheapest
  check, and only `--verify` executes it — T3 refuses to mark a guard
  confirmed unless it actually discriminates good from bad.
- T1 needs your `--test` command; T3 needs a good endpoint (auto-detected on
  a clean tree, or `--good`). Without both, the ladder reports the highest
  tier it *earned* (`likely` at best) instead of overclaiming.
- The failure memory is local, per-repo, and time-based: decay uses wall-clock
  days (not commit distance), and calibration *reports* tier agreement without
  yet reweighting tier wording — that feedback loop stays manual on purpose.

---

## License

MIT
