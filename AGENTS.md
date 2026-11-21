# senpai

senpai is the human-in-the-loop review client for coding agents. It must fail closed: an error, a malformed reviewer result or a change to the reviewed state never grants approval.

Keep the request/result JSON, exit codes, ledger file name and producer values, and the `TASK_REVIEW_*` and `HTML_REVIEW_*` variables compatible with existing callers unless a change is approved. Add a crate dependency only for a demonstrated need. Never add credentials, ledgers, live review URLs or session files.

Gate:

```sh
cargo clippy --release --all-targets -- -D warnings
cargo test --release
python3 -m unittest discover -s tests -v
node --test tests/test_diff_view.js
```

After changing the binary, run `cargo install --path .` so agents use the new build.

Do not create or publish a remote without explicit approval.
