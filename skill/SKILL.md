---
name: limit-watchdog
description: Keep background agent work going across Claude usage limits (the 5-hour limit) and stop cleanly at weekly limits. Use when the main session launches background agents meant to keep working while the user is away (overnight runs, long parallel streams), on every agent notification during such a run, when an agent "was stopped by a usage limit", when a "[limit-watchdog …]" timer prompt arrives, and when the user asks to resume, pause or check work around a limit reset.
---

# Limit watchdog (v2)

Nothing runs while a usage limit is active: not the main session, not subagents. Two things keep work going anyway:

1. **Waiting instead of failing.** The user `settings.json` sets `CLAUDE_CODE_RETRY_WATCHDOG=1` with a 5.5 h maximum wait, so a request that hits the 5-hour limit waits for the reset. A weekly reset is further away than 5.5 h, so it fails at once (that is wanted).
2. **A timer armed before the wall.** Desktop auto-continue resumes only the main thread, and only when the main thread itself hit the limit. An idle orchestrator whose subagents all failed gets no turn at the reset. So at about 80 % of the 5-hour window the main session arms a one-shot `CronCreate` wake for reset + 4 min, plus a Desktop scheduled task as a backup. When it fires, agents are resumed from a ledger **in batches of two**. Every resume after hours idle re-reads that agent's whole context uncached.

Design and test results: `C:\Users\moise\Dev\limit-watchdog\README.md`.

## Tools
- **`lw`** (`C:\Users\moise\.claude\skills\limit-watchdog\bin\lw.exe`; in Bash `/c/Users/moise/.claude/skills/limit-watchdog/bin/lw.exe`). It owns the ledger and every timing decision; never edit the ledger by hand. It finds this session's ledger from `CLAUDE_CODE_SESSION_ID`: `C:\Users\moise\.claude\work-ledger\<session-id>.json`. From any other session, pass `--ledger <full path>`. It prints JSON. `lw show` gives a summary.
- **Usage**: `mcp__ccd_session_mgmt__get_usage`. Pass these values to `lw`:
  - `--five-pct` / `--five-resets-at`: `percentUsed` / `resetsAt` of "5-hour limit".
  - `--weekly-pct` / `--weekly-resets-at`: the higher of "Weekly · all models" and the weekly row for the model the agents use.
- **Timers**:
  - `CronCreate` / `CronDelete`: session-only. They fire only while this session is open and idle, and one-shots auto-delete.
  - Desktop scheduled tasks (`mcp__scheduled-tasks__create_scheduled_task` with `fireAt`, `…__delete_scheduled_task`): these run as a new session and survive an app restart. A task's completion does **not** wake this session (tested 2026-11-15), so the backup task must message it.
- Load deferred tools with one `ToolSearch` call: `select:CronCreate,CronDelete,CronList,mcp__ccd_session_mgmt__get_usage,mcp__scheduled-tasks__create_scheduled_task,mcp__scheduled-tasks__delete_scheduled_task,SendMessage`.

## A. Before an unattended run
Check these once per run, and tell the user which ones are theirs to do:
- **The env block is in the user `settings.json`**: `CLAUDE_CODE_RETRY_WATCHDOG=1` and `CLAUDE_CODE_RETRY_WATCHDOG_MAX_USAGE_LIMIT_WAIT_MS=19800000`. Only sessions started after it was added have it.
- **The power plan never sleeps on AC** for the night (the user's action). Sleep or a closed app cancels every timer.
- **"Auto-continue when limits reset" is ticked** on the 5-hour limit card when one shows (the user's action). It helps only the main thread, so never rely on it.
- **Every command the agents need is pre-allowed**, or the session runs in Bypass mode. A permission prompt at 3 am stalls an agent until morning.
- **At most 4 background agents run at once while unattended** (`run.max_concurrent`).
- **Keep the orchestrator small.** A wake re-reads this session's whole context uncached. If `get_usage` reports a large context (over ~200k tokens), run the night from a fresh session that launches the agents itself, with a short brief plus the stream list. Agents can only be resumed by the session that launched them.

Then:
1. `lw init --project "<name and path>" --unattended`.
2. Add the **HANDOFF rule** to every brief, word for word:
   > Keep `HANDOFF.md` at your worktree root up to date at each work boundary (after each commit, test run or finished step): steps done, the current step, and any in-flight operation (command, file being edited, external request) with how to check whether it completed. If you are resumed, read it first, skip steps marked done, and re-check any in-flight operation before retrying it.
3. Write each agent's brief to a file (`<worktree>\BRIEF.md`) so it can be re-dispatched fresh.
4. After launching each agent, run `lw add --id <agentId> --name <stream> --agent-type <type> --kind resumable --risk <read-only|local-writes|irreversible> --worktree <path> --handoff <path>\HANDOFF.md --brief-file <path>\BRIEF.md`.
   - `--risk irreversible` means the agent pushes, deletes, sends, deploys or buys. Such an agent is never resumed automatically.
   - Explore and Plan agents are always `--kind redispatch`.

## B. Every turn while agents run
Each agent notification gives this session a turn. In that turn:
1. Call `get_usage`, then run `lw tick --five-pct … --five-resets-at … --weekly-pct … --weekly-resets-at …`.
2. Act on the result:
   - `"action":"arm"`: arm the timers (C).
   - `"armed"` or `"ok"`: nothing to arm.
   - `"weekly-warn"`: launch nothing more and tell the user.
   - `"stop_launching": true`: launch no new agents this window. The running ones carry on.
3. Act on the notification:
   - **Agent finished**: `lw set <id> done`.
   - **Agent stopped by a usage limit**: `lw stopped <id> --text "<the notification's limit line>"`, then `lw tick` again (it arms if nothing is armed yet).
   - **Agent failed for another reason**: `lw set <id> needs-human --note "<one line>"`. Never auto-retry it.
   - **User pauses something**: ask the agent to stop at a safe point and update HANDOFF.md, then `lw set <id> paused-by-user`.

## C. Arming the timers
`lw` returns `token`, `cron`, `wake_at`, `backup_at` and `for_reset`. Do all three steps:
1. **`CronCreate` one-shot.** Use `cron` as returned (local time, already moved off :00/:30), `recurring: false`, and this prompt:
   ```
   [limit-watchdog <token>] Follow the limit-watchdog skill, section D, for wake token <token>.
   ```
   Then run `lw timer-add --kind cron-oneshot --id <job id> --fires <wake_at> --token <token>`.
2. **Desktop backup task** (only for a `wall` wake, not a `batch` wake). Create it with `fireAt` = `backup_at`, name `limit-watchdog-<token>`, and the prompt in section E. Fill in the ledger's full path and `host_session_id`. Then run `lw timer-add --kind desktop-task --id <task id> --fires <backup_at> --token <token>`.
   - A task created from a Bypass-mode session is saved in Bypass mode. That is needed, because a Bypass session holds incoming messages from a non-bypass sender for approval it cannot show.
3. **Tell the user in one line**: "Armed: wake at <wake_at> for the <for_reset> reset."

## D. When a `[limit-watchdog <token>]` prompt arrives
1. Call `get_usage` first. A resume sent before the reset dies on the limit again.
2. Run `lw next --token <token> --five-pct … --five-resets-at … --weekly-pct … --weekly-resets-at …`.
3. Do exactly what the action says:
   - **`already-handled`**: the other timer got here first. Reply in one line and stop.
   - **`still-limited`**: arm again with the returned fields (C) and stop.
   - **`weekly-stop`**:
     - Run `lw toast "Limit watchdog" "<toast>"`.
     - Create one Desktop task at `notify_at` whose prompt is only: `Run "<lw path>" --ledger "<ledger path>" toast "Weekly limit reset" "Agents are waiting in <project>; resume them by hand."` Record it with `lw timer-add`.
     - Stop. Never resume after a weekly limit without the user's word.
   - **`herd-stop`**:
     - Run `lw toast`.
     - `CronDelete` the cron timers, delete the Desktop tasks, then `lw timers-clear`.
     - Stop and leave the rest for the user.
   - **`resume`**: for each `batch` item, in order:
     - **`mode: resume`**: `SendMessage` to the item `id` with its `resume` text.
     - **`mode: redispatch`**: launch a fresh background agent of `agent_type` with the brief from `brief_file`, plus "Read <handoff> first; skip steps marked done; re-check any in-flight operation before retrying it." Then run `lw set <old id> done --note "redispatched as <new id>"` and `lw add` the new agent with the same fields.
     - **If a send or launch fails on the limit**: `lw stopped <id> --text "<error>"`, then stop the batch.

     After the batch:
     - If `arm` is set, arm the next batch: `CronCreate` only, as in C1.
     - If `needs_human` is not empty, toast the names.
     - Toast the `toast` text.
   - **`done`**:
     - `CronDelete` / delete each id in `delete_timers`. A missing one is fine.
     - Toast and reply in one line.
4. Never start new work, merge, rebuild or widen a brief from a wake. Agents carry on with their own briefs. Their results are handled in later turns as usual (B).

## E. Desktop backup task prompt
```
[limit-watchdog backup <token>] Run: "C:\Users\moise\.claude\skills\limit-watchdog\bin\lw.exe" --ledger "<ledger path>" wake-status --token <token>
If "handled" is true, or "current" is false: reply "already handled" and stop.
Otherwise load mcp__ccd_session_mgmt__send_message with ToolSearch and send session <host_session_id> exactly:
[limit-watchdog <token>] Follow the limit-watchdog skill, section D, for wake token <token>.
If that fails or the session is gone, run lw with the same --ledger: toast "Limit watchdog" "Work did not resume after the limit: open the <project> session". Then stop.
Do not resume or launch agents yourself, and do nothing else.
```

## F. Never
- Resume `paused-by-user`, `done`, `needs-human` or `stopped-by-weekly` agents without the user's word.
- Read the login token, call usage endpoints directly, use another account, or turn on extra usage or credits.
- Run with permission checks off unless the user started the session that way.
- Read agent transcripts to decide anything. `lw size` checks the file size only.
- Leave timers behind when nothing is left to resume.
- Use a recurring cron as a safety net. Each fire re-reads the whole orchestrator context uncached.
