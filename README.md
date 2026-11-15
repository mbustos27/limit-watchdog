# limit-watchdog

Keeps long, multi-agent Claude Code work going across usage limits on this PC (Claude desktop app, Code tab, Windows 11).

Design source: the deep-research report `C:\Users\moise\Dev\fluentfin-mpv\docs\reports\Auto resume after Claude usage limits.md` ("Arm the wake-up before hitting the wall"). In short:

1. **Prevent**: `CLAUDE_CODE_RETRY_WATCHDOG=1` (+ a 5.5 h max wait) in the user `settings.json` `env` block, so requests and subagents *wait out* a 5-hour reset instead of failing — if the desktop app honours it (Phase 0, check 1).
2. **Arm early**: at about 80 % of the 5-hour window (read with the desktop app's `get_usage`), stop launching agents, and set a one-shot wake at reset + 4 min (off :00/:30) plus a desktop scheduled-task backup.
3. **Resume in batches**: from a per-session ledger, two agents at a time with a herd guard (re-check usage after each batch, stop if the window is filling); re-dispatch fresh from a handoff note when an agent's transcript is large; cap unattended concurrency at four.
4. **Weekly limits**: never wait them out automatically — mark the agents, notify, leave it to the user.
5. **Never**: read the login token, use other accounts, enable extra-usage credits, or run with permission checks off.

## Phase 0 checks (results)

| # | Check | Result | Date |
|---|---|---|---|
| 2 | `CronCreate` loads in a main Code-tab session | **Yes** — loaded and available in session e4cf1356 (session-only jobs; fire only while the session is open and idle; recurring jobs expire after 7 days) | 2026-11-15 |
| 3 | `get_usage` reset-time format | **ISO 8601 UTC** per window: `{"label": "5-hour limit", "percentUsed": 57, "resetsAt": "2026-11-15T23:30:00.194Z", "resetsIn": "4h 25m"}`; also "Weekly · all models", "Weekly · Fable" and `extraUsage` (enabled false). Plan reported: Pro. Also reports this session's context size (681k tokens = 68 % at the time) | 2026-11-15 |
| 1 | Desktop Code tab applies `env` from user `settings.json` | **Yes** — after adding `CLAUDE_CODE_RETRY_WATCHDOG=1` and `..._MAX_USAGE_LIMIT_WAIT_MS=19800000` to the user `settings.json` `env` block, a freshly started Code-tab session saw both in Bash and PowerShell (`WATCHDOG=[1] MAXWAIT=[19800000]`). Sessions started before the change don't have them. Whether subagents then *wait* instead of failing is only proven at the next real limit | 2026-11-15 |
| 4 | A desktop scheduled task (`fireAt` 3 min out) wakes an idle session | **No** — the task fired on time (11:11:08 for 11:11:00), ran as its own new session and succeeded, but its completion notice did **not** start a turn in the idle main session (nothing happened until the user typed). Note: tasks created from a session in Bypass mode are saved in Bypass mode. Not tested: the task actively messaging the session (`send_message`), which unattended runs may be refused | 2026-11-15 |
| 4b | An in-session `CronCreate` one-shot wakes the idle main session | **Yes** — a one-shot for 11:16 (`16 11 15 11 *`, `recurring: false`) started a turn in the idle session at 11:16:02 with no user input; it then auto-deleted. This is the primary wake path (session must be open, PC awake) | 2026-11-15 |
| 5 | Main session gets a turn after the limit-failure notice | observed once (2026-11-15 ~03:40): failure notices arrived with a short "grace allowance"; not documented, so not relied on | 2026-11-15 |

**Context-size lesson (2026-11-15):** the orchestrating session had grown to ~681k tokens. A wake after a 5-hour gap re-reads the whole context uncached, so long unattended runs should be orchestrated from a fresh, small session (hand off with a short brief + the ledger), not from a long interactive one.

## Phase 1 configuration (status)

| Item | State |
|---|---|
| Retry-watchdog `env` block in `C:\Users\moise\.claude\settings.json` | Done 2026-11-15 (check 1). Whether a subagent really *waits* is proven only at the next real 5-hour limit |
| Power plan never sleeps on AC | Already so: High performance, sleep and hibernate "never" on AC (read 2026-11-15) |
| "Auto-continue when limits reset" checkbox | The user's action, on the 5-hour limit card when one shows. Main thread only; never relied on |
| `permissions.allow` coverage | The orchestrating session runs in Bypass mode and its background agents inherit it, so no prompts stall a night. A session in another mode needs the allowlist audited first |
| Unattended cap of 4 | `lw` default `run.max_concurrent = 4` |
| HANDOFF rule in agent briefs | Skill section A, step 2 (pasted into every brief) |

## Layout
- `skill\SKILL.md` — the procedure (v2). Installed copy: `C:\Users\moise\.claude\skills\limit-watchdog\SKILL.md`, with `bin\lw.exe` beside it.
- `lw\` — the Rust helper: per-session ledger (atomic writes under a lock), reset-time parser, arm/next decisions with batches and the herd guard, transcript size check (file size only), toasts.
- `ledger.example.json` — ledger format (v2), matching `lw`.
- Ledgers live in `C:\Users\moise\.claude\work-ledger\<session-id>.json`, one per orchestrating session.
