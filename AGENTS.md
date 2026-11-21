# senpai

senpai is the human-in-the-loop review client for coding agents. It must fail closed: an error, a malformed reviewer result or a change to the reviewed state never grants approval.

Keep the request/result JSON, exit codes, ledger file name and `TASK_REVIEW_*` variables compatible with existing callers unless a change is approved. Use only the Python standard library at runtime. Never add credentials, ledgers, live review URLs or session files.

Gate:

```sh
python3 -m unittest discover -s tests -v
node --test tests/test_diff_view.js
sh -n bin/senpai
```

Do not create or publish a remote without explicit approval.
