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

### Inbox

The built-in HTML reviewer sends every review to one local inbox. The first
review starts `senpai serve` in the background, and later reviews from any
agent session join the same page. The service stops itself after 10 minutes
with nothing waiting.

```sh
"$SENPAI" inbox                       # open the inbox page; prints {"inbox": URL, "pending": N}
"$SENPAI" serve --stop                # stop the service; waiting reviews fail with exit 2
"$SENPAI" ask --question "Which port should the dev server use?" \
  --option 8080 --option 9090 --context /path/to/repo
```

- **Same contract.** `review` still blocks and returns the same JSON and exit codes. The client still fingerprints, rechecks and writes the ledger; the service only queues, shows and relays. If it dies or is stopped, the review fails with exit 2 and nothing is approved.
- **Withdrawal.** A client that ends early (interrupt, timeout, killed) withdraws its item. Closing a review tab does not: the item stays until you decide, within the usual `HTML_REVIEW_LIFETIME` and `HTML_REVIEW_IDLE` page limits.
- **Attention.** A new item opens the inbox in the browser, or shows a macOS notification when the inbox is already open. `HTML_REVIEW_NO_BROWSER=1` turns both off. Inside Rex it opens in a split instead (see below).
- **`ask`.** Prints `{"version": "1", "question_id": N, "option": …, "text": …}`. Exit 0 answered, 1 dismissed, 2 error or `TASK_REVIEW_TIMEOUT`. No ledger is written.
- **One-off page.** `SENPAI_INBOX=0` gives each review its own page and server, as before the inbox.
- **Files.** `~/Library/Application Support/senpai/` (mode 0700): `senpai.sock`, `serve.lock`, `serve.log`. `SENPAI_HOME` moves them; a Unix socket path must stay under 104 bytes. `SENPAI_SERVE_IDLE` sets the idle exit in seconds.
- **Updates.** After `cargo install`, an idle service from the old build restarts on the next request. One with items still waiting keeps running, and new requests fail until those items finish or you run `senpai serve --stop`.

### Tern

When an agent runs in a Tern pane (`TERN_PANE` is set), each new review opens the
real review page in a Tern browser block docked beside that pane, with the same
HTML, CSS and mouse support as the browser; a question opens the inbox there.
The block closes once the item is decided or its agent goes away. If Tern can't
show it, the item still waits in the inbox and the browser opens instead. Tern
and Rex panes open even with `HTML_REVIEW_NO_BROWSER=1`; use
`SENPAI_SURFACE=none` to turn them off.

### Terminal and Rex

`senpai tui` shows the inbox in a terminal and works with keys or the mouse:
click a line or drag across lines to select them, click **Comment**, then
**Approve**, **Request changes** or **Return to planning**. Every decision asks
for confirmation first. `senpai tui --item ID` opens one item and exits once it
is decided, here or in the browser. Press `?` for all keys.

- **Inside Rex.** When an agent runs in a Rex pane (`REX_SESSION` and `REX_BLOCK` are set), each new review opens in a split beside that pane instead of a browser tab. With `terminal-browser` installed, the split shows the real review page (a question shows the inbox); otherwise it runs `senpai tui --item ID`. The split closes after the decision. If neither can open, the item still waits in the inbox and the browser opens instead. `SENPAI_TERMINAL_BROWSER` names the program, or `none` to always use `senpai tui`.
- **`SENPAI_SURFACE`** chooses where new items appear: `auto` (default: Tern inside Tern, Rex inside Rex, else the browser), `browser`, `tern`, `rex` or `none`.
- **Same checks.** The terminal sends decisions to the service, which checks them exactly like the page's. Document reviews show the markdown source lines, so comments land on the same lines. The Guided tab and syntax colors stay in the browser.

### Privacy and trust

| Boundary | Rule |
| --- | --- |
| Browser | `127.0.0.1`, session token, same-origin JS/fonts; **keep the URL private** |
| Inbox | Private directory and socket; client and service each refuse a peer run by another user; every review and the inbox page have their own random token |
| Tern | The page opens in a browser block the agent's own `tern browser` commands can drive, so an agent could click it; like the URL itself, this is not a barrier against a local agent |
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
overrides it). It turns the inbox off for every process it starts, except
`tests/test_inbox.py`, which runs its own service in a private `SENPAI_HOME`,
and hides any Rex or Tern session it runs in; Rex and Tern are tested with fake `rex` and `tern` commands. `tests/test_rust_parity.py` also compares it with the last Python
client, extracted from git history. Golden cases recorded from that client live
in `tests/golden/` and are replayed by `cargo test`.

No GitHub Actions workflow is provided; run the checks locally.

## License and assets

- Owner-controlled code: [MIT](LICENSE).
- Vendors retain their own MIT, ISC, BSD, Apache, dual MPL/Apache and OFL terms: [NOTICE](NOTICE).
- Builds, hashes, existing modifications and version limits: [asset provenance](assets/PROVENANCE.md).
