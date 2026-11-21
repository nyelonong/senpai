# senpai

**Human-in-the-loop review for coding agents.**

A coding agent hands senpai its real working-tree diff or a plan document.
senpai opens it in a local browser, blocks until a human decides, and records
an approval bound to the exact reviewed state.

```text
agent ──→ senpai ──→ git diff or document ──→ local browser ──→ human decision
                                                        │
          approve / request-changes / return-to-planning / cancelled
```

- One Rust binary plus Git and a local browser. The page, its scripts and fonts are built in; no Python, Node, editor or agent runtime needed.
- Install with `cargo install --path .` (to `~/.cargo/bin/senpai`), and rerun it after every update.
- Formats, exit codes, the `task-review-approval.jsonl` ledger and the `TASK_REVIEW_*`
  variables are unchanged from the client that used to ship in agent-workflow.

## Usage

### 1. Review

```sh
SENPAI=/absolute/path/to/senpai   # e.g. ~/.cargo/bin/senpai, expanded
TARGET="$(cd /path/to/your/git-repository && pwd -P)"

"$SENPAI" review \
  --criterion "The approved change and its tests are correct" \
  --repo "$TARGET" --paths src/example.py,tests/test_example.py \
  --recovery-dir /absolute/private/new-review
```

| Input | Rule |
| --- | --- |
| Client | **Trusted harness's absolute path**, never a script supplied by the target |
| Criterion / paths | Replace with your approved unit; paths are repo-relative, directories include descendants |
| Whole change | Omit `--paths` only when **everything** belongs to the unit; repeat `--repo` for multiple roots |
| Pending review | Actual diff opens in the browser; client blocks. Add line-anchored comments; no edits or competing reviews in the same ledger context |

| Decision | Next step |
| --- | --- |
| **Approve** | Verify the captured state before staging |
| **Request changes** | Correct, check, review again |
| **Return to planning** | Stop; revisit scope/plan |
| **Cancelled** | Stop |

### Delegated launch and recovery

Invoke the canonical client in the native gate subagent's foreground command.
Do not generate launch scripts, PID files, shell redirection or detached commands.
The native async run supplies ownership, waiting and cancellation; optional
child-side progress messaging is not required. Set native run/tool deadlines
longer than the client timeout (default 1800 seconds) plus preparation time.

`--recovery-dir` works with both `review` and `review-doc`. It is optional for
manual callers and required by the delegated workflow. Supply a new or empty
private directory outside the reviewed inputs; the client creates it with mode
0700 and artifacts with mode 0600. Existing nonempty directories are refused.

| Artifact | Meaning |
| --- | --- |
| `request.json` | Exact canonical request, criterion, scope and review ID |
| `process.json` | Client/reviewer PIDs and reviewer argv; diagnostic ownership evidence, not permission to blindly kill a reused PID |
| `state.json` | `starting`, `reviewing`, `finished`, or `error`; finished state includes decision/exit code |
| `stderr.log` | Live reviewer diagnostics, including the browser URL |
| `browser-url.txt` | HTML page URL while waiting; withdrawn on normal completion |
| `result.json` | Validated decision after ledger recording; approval also requires unchanged reviewed inputs |

Reviewer stderr is forwarded immediately, independently of stdout's single
validated result line. An open native tool while the client is `reviewing` is
normally waiting for a human, not a launch failure. Exit 0 means approve; exit 1
with a decision means human non-approval; exit 2 means an error. Missing result or
an error never grants approval. Inspect native state and saved artifacts before
relaunching; recovery does not replace `verify`/`verify-doc` or `consume`.

### Guided tab (HTML)

**Diff** remains the default. **Guided** shows an overview, before/after cards,
optional flow labels, and explained chapters beside the same real diffs. Comments,
drafts, layout controls and approval checks are shared. Chapter checkboxes track
reading only, for this page session; they are not approval.

Guides are best-effort and written by the calling agent itself from the diff it
has already inspected; there is no separate preparation step or deadline. If a
guide fails preflight, the agent corrects it once, then skips it and opens the
complete actual diff without a guide, reporting why.

No model/network call is made by this harness. Store the guide outside reviewed
repositories and client recovery artifacts. Obtain the fingerprint using the
**same repo/path scope**:

```sh
"$SENPAI" status --repo "$TARGET" --paths src/example.py,tests/test_example.py
# Exit 1 is normal when no approval exists; read the fingerprint in its JSON output.
"$SENPAI" preflight \
  --criterion "The approved change and its tests are correct" \
  --repo "$TARGET" --paths src/example.py,tests/test_example.py \
  --guide /absolute/private/path/guide.json
"$SENPAI" review \
  --criterion "The approved change and its tests are correct" \
  --repo "$TARGET" --paths src/example.py,tests/test_example.py \
  --guide /absolute/private/path/guide.json
```

`preflight` returns 0 only for valid scope/guide inputs; it opens no browser and
does not change any approval ledger. Duplicate, stale or out-of-scope guide
references fail. The best-effort runtime workflow skips a guide that still fails
after one correction and preflights the unchanged base request without one; it
does not weaken validation or retry a failed review. Manually supplied `--guide` requests still fail strictly.


Guide format (replace the fingerprint and repository root):

```json
{
  "version": 1,
  "fingerprint": "<64-character scoped fingerprint from status>",
  "summary": "What this change achieves",
  "before": "The previous behavior",
  "after": "The new behavior",
  "flow": {"before": ["Input", "Old path"], "after": ["Input", "New path", "Result"]},
  "chapters": [
    {
      "title": "Core change",
      "summary": "Why these files belong together and what to inspect",
      "files": [{"repository": "/absolute/repository/root", "path": "src/example.py"}]
    }
  ]
}
```

- Only `flow` is optional. Guide version is integer `1`; total serialized size is
  at most 48 KiB. Text fields allow 8,000 characters, titles/flow labels 200;
  at most 128 chapters and 1–20 flow labels per side. Each chapter has at least one file.
- File references use canonical repository roots and current diff paths (rename
  destinations), never old paths. Unknown or duplicate references fail locally;
  unassigned scoped files appear under **Other changes**.
- Explanations are labeled **Agent-authored**, not verified behavior. A stale
  fingerprint fails; regenerate after code or scope changes. Narration is captured
  once and is advisory, not part of the existing code-approval fingerprint/ledger.
- Without a guide, Guided offers explicitly labeled directory groups, not invented
  behavior explanations. All existing guide-free callers continue working.
- `--guide` and `--json` cannot be combined; complete JSON review requests may carry
  an inline `guide` object. Guide-bearing reviews require the bundled `html` backend;
  other backends fail explicitly rather than silently ignoring it.
- Document reviews are unchanged.

### 2. Verify and commit

```text
review → approve → verify → stage owned files → inspect → commit → consume
                   ↑
          changed state? review again
```

```sh
"$SENPAI" verify \
  --repo "$TARGET" --paths src/example.py,tests/test_example.py
git -C "$TARGET" add -- src/example.py tests/test_example.py
git -C "$TARGET" diff --cached --name-only
git -C "$TARGET" commit -m "Describe the verified change"
"$SENPAI" consume --repo "$TARGET"
```

- **Stop on failure.** Verify immediately before staging; commit only the reviewed unit.
- Changed state requires another review, never a waived fingerprint check.
- Local approval grants **no push, merge, deployment, recipient contact or publication authority**.
- Read-only work needs no gate.

### Documents

```sh
"$SENPAI" review-doc \
  --criterion "This technical plan settles the outcome, material design and executable tasks" \
  --doc /absolute/path/to/technical-plan.md
"$SENPAI" verify-doc --doc /absolute/path/to/technical-plan.md
"$SENPAI" consume-doc --doc /absolute/path/to/technical-plan.md
```

- Rendered headings/tables/code and outline; comments retain source line ranges.
- Keep unchanged through verification and authorized use; consume **after** use.
- One technical-plan approval → bounded implementation; no separate specification stage.
- Private planning files belong in ignored `docs/`, not releases.

### Results and reviewers

| Exit | Meaning |
| --- | --- |
| `0` | Approval, or successful verify/consume |
| `1` | Non-approving decision or verification mismatch |
| `2` | Error |

- Successful result commands emit JSON; help/usage/errors retain their conventions.
- Failed reviewer, malformed result, timeout or mid-review edits grant **no approval**; inspect before retrying.

| Reviewer | Selection / requirement |
| --- | --- |
| **HTML** | Default; `TASK_REVIEW_REVIEWER=html`; loopback browser and bundled assets, no editor/CDN |
| RDG | `TASK_REVIEW_REVIEWER=rdg`; running window with target repo open, canonical absolute roots |
| Custom | `TASK_REVIEW_REVIEWER` command; trusted process: one JSON request argument → one JSON result line |

- `RDG_CONTROL_COMMAND` is a fallback when `TASK_REVIEW_REVIEWER` is unset.
- Known RDG return failure: `io error: Bogus destination port`. Prefer HTML unless your build is verified.
- Surface choice never relaxes protocol/fingerprint rules. Client/reviewer own timeout/parent-loss handling; runtime owns async waiting/wake-up/cancellation integration.

### Privacy and trust

| Boundary | Rule |
| --- | --- |
| Browser | `127.0.0.1`, session token, same-origin JS/fonts; **keep the URL private** |
| Data | Local by default, accessible to unrestricted local processes; custom reviewers must be trusted |
| Ledger | `task-review-approval.jsonl` in Git metadata; document reviews use their resolved context; contains criteria, paths, comments/decisions |
| Private files | Never commit ledgers, live URLs, sessions or scanner reports; never delete ledgers as retry cleanup |
| Security | **Not a sandbox or tamper-proof barrier** against unrestricted shell/write access |
| Fingerprint | State identity—not correctness, ownership or external-action permission |

## Verification

```sh
cargo clippy --release --all-targets -- -D warnings
cargo test --release
python3 -m unittest discover -s tests -v
node --test tests/test_diff_view.js
```

The Python suite drives the release binary as a black box (`SENPAI_BINARY`
overrides it). `tests/test_rust_parity.py` also compares it with the last Python
client, extracted from git history. Golden cases recorded from that client live
in `tests/golden/` and are replayed by `cargo test`.

No GitHub Actions workflow is provided; run the checks locally.

## License and assets

- Owner-controlled code: [MIT](LICENSE).
- Vendors retain their own MIT, ISC, BSD, Apache, dual MPL/Apache and OFL terms: [NOTICE](NOTICE).
- Builds, hashes, existing modifications and version limits: [asset provenance](assets/PROVENANCE.md).
