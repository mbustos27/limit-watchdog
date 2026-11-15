# lw

The limit watchdog's helper. The `limit-watchdog` skill calls `lw` instead of editing JSON by hand. `lw` keeps one ledger per orchestrating session (the background agents, their states, the current wake and its timers), and it makes every timing decision for resuming agents after a Claude usage-limit reset. It never reads a transcript: `size` looks at file metadata only.

Design: `..\README.md` and the report `C:\Users\moise\Dev\fluentfin-mpv\docs\reports\Auto resume after Claude usage limits.md` (section "Limit watchdog v2").

## Build

This PC builds Rust with the GNU toolchain (`stable-x86_64-pc-windows-gnu`) plus WinLibs for `gcc`/`dlltool`, the same setup as `library-rs` and `jbod-suite-rs`.

```powershell
.\gate.ps1                 # cargo fmt --check, clippy -D warnings, all tests (adds WinLibs to PATH itself)
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:LOCALAPPDATA\Programs\winlibs\mingw64\bin;" + $env:Path
cargo build --release      # target\release\lw.exe, self-contained (runs without WinLibs on PATH)
```

Dependencies: serde, serde_json, chrono, chrono-tz, and the `windows` crate for the toast (WinRT `ToastNotificationManager`, as in the JBOD suite and inbox-ledger; no PowerShell).

## Ledger

- Path: `--ledger PATH`, else `%USERPROFILE%\.claude\work-ledger\<session>.json`. The session is `--session ID` or `CLAUDE_CODE_SESSION_ID`. `init` also records `CLAUDE_CODE_HOST_SESSION_ID` (the `local_...` id that `send_message` needs).
- Every change runs under `<ledger>.lock`. The lock is an exclusive create; lw waits up to 10 s for it and treats a lock older than 60 s as stale. The ledger is written to a temp file in the same folder, flushed, then renamed over the old file. Read-only commands (`show`, `wake-status`, `size`) take no lock.
- Schema version 2: see `..\ledger.example.json`, which lw itself generated. One addition to the spec: `run.wake_seq` counts the wakes created so far. Tokens are `w<wake_seq>`, so a token is never reused, even after `done` clears the wake.
- The log keeps the newest 500 lines.
- States: `running`, `stopped-by-limit`, `stopped-by-weekly`, `needs-human`, `paused-by-user` and `done`. Explore and Plan agents are always `redispatch`.

## Commands

Every command prints one line of JSON (keys in alphabetical order), except `show` and `help`.

Exit codes:
- 0: success.
- 2: a wrong command line (an unknown option, a bad state word, a missing value).
- 1: any other failure (no ledger, duplicate id, unrecognised limit text, locked for over 10 s).

Errors go to stderr, prefixed `lw: `. The global `--now ISO` replaces the clock, for tests.

| Command | Does |
|---|---|
| `init [--project TEXT] [--unattended]` | Create the ledger; refuses if one exists. |
| `add --id --name --agent-type --kind --risk [--worktree] [--handoff] [--brief-file] [--resume]` | New agent as `running`. Refuses a duplicate id, or an id other than letters, digits, `-` and `_`. Explore/Plan are forced to `redispatch`, with a `note`. Default resume text: "Resume from <handoff>. Skip steps marked done; re-check any in-flight operation before retrying it." |
| `set ID STATE [--note TEXT]` | State change, logged. |
| `stopped ID --text "<notice>"` | "weekly" → `stopped-by-weekly`; 5-hour / session / usage limit → `stopped-by-limit`; anything else → exit 1, so record it with `set` instead. Stores `last_failure` and outputs `reset` (`{reset, wake_at, cron, backup_at}`, or null if the text has no readable reset). If the agent was resumed less than `herd_refail_min` minutes ago, it sets `run.herd_tripped`, and the output has `herd_tripped: true` and a `toast`. |
| `reset-time "<text or ISO>"` | Pure parser → `{reset, wake_at, cron, backup_at}`. |
| `tick --five-pct N --five-resets-at ISO [--weekly-pct N --weekly-resets-at ISO]` | See "Decisions". |
| `next --token T` + the same usage flags | See "Decisions". |
| `timer-add --kind cron-oneshot\|desktop-task --id ID --fires ISO --token T` | Record a timer. The same id replaces the old entry. Warns if T is not the current wake. |
| `wake-status --token T` | Read-only: `{handled, current, host_session_id, session_id, pending_agents, reason, wake_at, current_token}`. `handled` is true when T is not the current wake, or when it is current and already stamped. |
| `timers-clear` | `{delete_timers:[ids]}`, then empties the list. |
| `size ID` | `{id, path, bytes, mode, why, in_ledger}` from `...\projects\*\<session_id>\subagents\agent-<ID>.jsonl` metadata. A missing file gives bytes null and mode `redispatch`. |
| `run-set KEY VALUE` | One `run` field, validated: percentages 1–100, counts ≥ 1, bools true/false. `cycle_base_pct` takes `null`. `-` and `_` both work in key names. |
| `new-night` | Zeroes `wake_cycles`, `cycle_base_pct`, `herd_tripped` and every agent's `resumes_tonight`. |
| `show [--json]` | Summary, or the whole ledger. |
| `log TEXT` | Append a log line. |
| `toast TITLE BODY` | Show a Windows toast and log it. Always exits 0: if the toast fails, or there is no ledger, `warnings` says so (also on stderr). |
| `toast-setup [--remove]` | Write (or remove) `HKCU\Software\Classes\AppUserModelId\LimitWatchdog.lw`. Windows shows toasts only for a registered app id. Needs no admin; run it once per PC. |
| `migrate-v1 PATH` | Turn a version 1 file into a new v2 ledger at the usual path. The v1 file has `{"agents":[{id, stream, worktree, state, resume}]}` or a bare list. Each agent gets kind `resumable`, risk `local-writes` and an empty agent type; an unknown state becomes `needs-human`. Refuses to write over an existing ledger or the v1 file itself. |

### Times

- Output times are RFC 3339 in local time (`2026-11-16T03:44:00-08:00`). `cron` is `"M H DoM Mon *"` for `wake_at`, local.
- Accepted reset input:
  - ISO with or without millis or `Z`
  - `resets 3:40am (America/Los_Angeles)` (any IANA zone)
  - `resets 3:45pm` (local)
  - `resets Mon 12:00am`
  - `resets Nov 17, 10am`
  - `resets in 4h 25m`
  - `noon` / `midnight`
  - text before "resets" (it is ignored)
  - anything after a `·` separator or after the time (also ignored)
- Date rules: a clock time earlier than now means tomorrow; a weekday means its next occurrence (a week ahead if that time has passed today); a month and day already past means next year.
- `wake_at` = reset + 4 min and `backup_at` = reset + 10 min. The reset's milliseconds are dropped first, each time is rounded up to a whole minute, then moved one minute on if it lands on :00 or :30. A wake is never set in the past: a reset that has already gone gets a wake 2 minutes from now.
- Two reset times within 2 minutes of each other count as the same reset, because `get_usage` milliseconds drift between calls.

## Decisions

**`tick`**, on each orchestrator turn:
- **`herd-stopped`**: the herd guard is tripped. Nothing is armed until a person clears it (`run-set herd_tripped false` or `new-night`).
- **`arm`**: an agent is running or stopped-by-limit, and either 5-hour ≥ `arm_at_pct` or an agent is stopped-by-limit. lw creates a wall wake → `{token, reason, cron, wake_at, backup_at, for_reset, stop_launching: true, weekly_warn, delete_timers, toast}`.
- **`armed`**: an unhandled wake already exists for the same reset. Same fields; nothing changes.
- **`weekly-warn`**: weekly ≥ `weekly_stop_pct`, with nothing to arm. When lw arms, the arm output carries `weekly_warn: true` instead.
- **`ok`**: none of the above; `stop_launching` is true when 5-hour ≥ `arm_at_pct`.

**`next`**, when a timer fires, in this order:
1. **`already-handled`**: the token is not the current wake, or the wake is already stamped. Nothing changes.
2. lw stamps `handled_at`. A wall wake also adds 1 to `wake_cycles` and sets `cycle_base_pct` to the current 5-hour %.
   - A weekly wake (the notice after a weekly stop) returns **`weekly-reset`** `{waiting, toast}` and resumes nothing.
   - With `resume_after_weekly`, a weekly wake instead moves stopped-by-weekly agents back to stopped-by-limit and carries on with step 3.
3. **`still-limited`**: 5-hour ≥ 100. lw re-arms a wall wake for the new reset, unless `wake_cycles` ≥ `max_wake_cycles`, which is a herd stop.
4. **`weekly-stop`**: weekly ≥ 100, or ≥ `weekly_stop_pct` without `resume_after_weekly`.
   - Every stopped-by-limit agent becomes stopped-by-weekly.
   - lw creates a weekly wake at weekly reset + 15 min → `{weekly_pct, weekly_resets_at, notify_at, cron, token, agents, toast}`.
5. **`herd-stop`** `{reason, waiting, toast}` when any of these holds:
   - `herd_tripped` is set
   - 5-hour − `cycle_base_pct` > `herd_rise_pct`
   - `wake_cycles` > `max_wake_cycles`

   A herd stop also sets `herd_tripped`, so later ticks don't arm again.
6. Each stopped-by-limit agent that is `irreversible`, or has `resumes_tonight` ≥ `max_resumes_per_agent`, becomes `needs-human` (listed in `needs_human` with a `why`).
7. **`resume`**: lw takes the remaining stopped-by-limit agents, read-only first, then the smallest transcript (none counts as 0), then ledger order. The batch size is min(`batch_size`, `max_concurrent` − running).
   - Each agent gets a `mode`: `redispatch` if its kind is redispatch, its transcript is over `redispatch_over_bytes`, or no transcript was found; else `resume`.
   - Each agent is set to `running`, gets `resumes_tonight` + 1 and `last_resumed` = now, and is logged.
   - If more remain, lw creates a batch wake at now + `batch_gap_min` → `arm: {token, cron, wake_at, backup_at}`, else `arm: null`.
   - Output: `{batch:[{id, name, mode, why, resume, agent_type, brief_file, handoff, worktree, transcript_bytes}], needs_human, remaining, arm, delete_timers, toast}`.
8. **`done`**: nothing is stopped-by-limit → `{delete_timers, needs_human, toast}`. lw clears `timers` and `wake`.

**`delete_timers`**: any output that has it (`arm`, `still-limited`, `weekly-stop`, `herd-stop`, `resume`, `done`, `weekly-reset`, `timers-clear`) lists timers that belong to an older or finished wake. Delete each one (a missing one is fine). lw has already dropped them from the ledger.

**`toast`** in an output is the body text. The caller picks the title, for example `lw toast "Limit watchdog" "<toast>"`.

## Example flow

```powershell
lw init --project "fluentfin-mpv (C:\Users\moise\Dev\fluentfin-mpv)" --unattended
lw add --id a1b2c3 --name bridge-tests --agent-type general-purpose --kind resumable --risk local-writes `
       --worktree C:\wt\bridge --handoff C:\wt\bridge\HANDOFF.md --brief-file C:\wt\bridge\BRIEF.md
# every turn: get_usage, then
lw tick --five-pct 82 --five-resets-at 2026-11-16T11:40:00.194Z --weekly-pct 41 --weekly-resets-at 2026-11-20T17:00:00Z
#  {"action":"arm","token":"w1","cron":"44 3 16 11 *","wake_at":"2026-11-16T03:44:00-08:00","backup_at":"2026-11-16T03:50:00-08:00",...}
lw timer-add --kind cron-oneshot --id <CronCreate id> --fires 2026-11-16T03:44:00-08:00 --token w1
lw timer-add --kind desktop-task --id limit-watchdog-w1 --fires 2026-11-16T03:50:00-08:00 --token w1
lw stopped a1b2c3 --text "You've hit your 5-hour limit · resets 3:40am (America/Los_Angeles)"
# 03:44, the one-shot fires: get_usage, then
lw next --token w1 --five-pct 1 --five-resets-at 2026-11-16T16:40:00.000Z
#  {"action":"resume","batch":[{"id":"a1b2c3","mode":"resume","resume":"Resume from C:\\wt\\bridge\\HANDOFF.md. ..."}],
#   "remaining":0,"arm":null,"delete_timers":["<cron id>","limit-watchdog-w1"],"toast":"Resumed 1: bridge-tests."}
# 03:50, the backup task checks first and finds it handled:
lw --ledger <path> wake-status --token w1     #  {"handled":true,...}
```

## Test hooks (environment)

- `LW_TZ`: an IANA zone used instead of the system zone.
- `LW_PROJECTS_DIR`: the transcripts root, instead of `%USERPROFILE%\.claude\projects`.
- `LW_TOAST_FILE`: append each toast to this file as a line of JSON instead of showing it.
- `LW_TOAST_FAIL`: make delivery fail.

The integration tests (`tests\cli.rs`) set all four, plus `USERPROFILE`, to a temp folder. They never touch the real `.claude` folder and never show a toast.
