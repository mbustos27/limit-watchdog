# limit-watchdog

Keeps Claude Code background agents working across the **5-hour usage limit** and stops cleanly at **weekly limits**. It is a Claude Code skill plus `lw`, a small Rust helper that keeps the ledger and makes every timing decision.

It was built for the Claude desktop app's Code tab on Windows, for long runs where the main session launches several background agents and the user walks away, for example overnight.

## The problem

When a usage limit lands, background subagents fail. Claude Code's built-in auto-continue (on by default in the CLI, and a checkbox on the desktop app's limit card) resumes only the **main thread**, and only when the main thread itself hit the limit. An idle orchestrator whose subagents all failed gets no turn at the reset, so nothing resumes until someone types.

The stopped agents lose nothing: a subagent resumes from its saved transcript when the main session sends it a message (`SendMessage` to its agent id). What is missing is a turn at the right time, plus a record of which agents to resume and how.

## How it works

1. **Wait instead of failing.** With `CLAUDE_CODE_RETRY_WATCHDOG=1` and a 5.5-hour maximum wait, a request that hits the 5-hour limit waits for the reset instead of failing. A weekly reset is further away than the cap, so it fails at once, which is what you want.
2. **Arm before the wall.** On each turn of an unattended run, the session reads the desktop app's usage figures. At about 80 % of the 5-hour window (or when an agent is stopped by the limit), it stops launching agents and arms two timers:
   - a one-shot `CronCreate` wake at reset + 4 minutes, moved off :00 and :30, which is the primary wake
   - a desktop scheduled task a few minutes later, as a backup that survives an app restart
3. **Resume in batches.** When the wake fires, `lw` picks what to resume from a per-session ledger:
   - Two agents at a time, read-only ones first, then the smallest. Every resume after hours idle re-reads that agent's whole context uncached, so resuming everything at once can burn a fresh window in minutes.
   - An agent whose transcript is large is re-dispatched fresh from its handoff note instead of resumed.
   - A herd guard stops the run if usage climbs too fast, a resumed agent fails again right away, or the run passes its wake-cycle limit.
   - An agent marked `irreversible` (it pushes, deletes, sends, deploys or buys) is never resumed automatically.
4. **Weekly limits notify only.** Agents are marked, a Windows toast says so, and resuming is left to the user.

Every arm, wake, resume and stop shows a Windows toast and is written to the ledger's log.

## What it never does

- Read the login token, call usage endpoints directly, or use another account.
- Turn on extra usage or credits.
- Read transcript contents (`lw size` looks at file size only).
- Resume an agent the user paused or one that finished, or widen an agent's brief.
- Use a recurring cron as a safety net: each fire would re-read the whole orchestrator context uncached.

Waiting for a reset and continuing your own queued work with Claude Code's own tools is how the product is meant to be used. This tool doesn't try to get around limits.

## Requirements

- **Platform:** Windows 10 or 11, and the Claude desktop app with Claude Code (Code tab). Tested with desktop app 5.11492 and Claude Code 2.1.325.
- **Tools the session needs:**
  - `CronCreate`
  - the desktop app's session-management `get_usage` and `send_message`
  - desktop scheduled tasks
- **Rust**, to build `lw`. It's built and tested with the GNU toolchain (`stable-x86_64-pc-windows-gnu` plus WinLibs); MSVC is untested. See [`lw/README.md`](lw/README.md).
- **Keep the app open and the PC awake.** In-session timers die with the app, and sleep cancels every timer. Set the power plan to never sleep on AC for unattended runs.

## Install

1. Build `lw`:
   ```powershell
   cd lw
   .\gate.ps1              # format check, clippy, all tests
   cargo build --release   # lw\target\release\lw.exe
   ```
2. Install the skill:
   ```powershell
   $dst = "$env:USERPROFILE\.claude\skills\limit-watchdog"
   New-Item -ItemType Directory -Force "$dst\bin" | Out-Null
   Copy-Item skill\SKILL.md "$dst\SKILL.md"
   Copy-Item lw\target\release\lw.exe "$dst\bin\lw.exe"
   & "$dst\bin\lw.exe" toast-setup   # once per PC: registers the toast app id under HKCU
   ```
3. Add the following to `%USERPROFILE%\.claude\settings.json`, merging with what is there:
   ```json
   {
     "env": {
       "CLAUDE_CODE_RETRY_WATCHDOG": "1",
       "CLAUDE_CODE_RETRY_WATCHDOG_MAX_USAGE_LIMIT_WAIT_MS": "19800000"
     },
     "permissions": {
       "allow": [
         "Bash(~/.claude/skills/limit-watchdog/bin/lw.exe:*)",
         "mcp__ccd_session_mgmt__send_message"
       ]
     }
   }
   ```
   - The `env` block applies only to sessions started after you add it.
   - The `permissions` entries matter for the backup task (see "Tested behaviour" below).
4. Optional: tick "Auto-continue when limits reset" on the desktop app's 5-hour limit card when it shows. It helps the main thread only.

## Use

Ask Claude to run work unattended with the watchdog, for example "run these four streams overnight; use the limit watchdog". The skill then:
- creates a ledger (`%USERPROFILE%\.claude\work-ledger\<session-id>.json`)
- registers each background agent with a kind and a risk class
- adds a handoff rule to every brief: keep `HANDOFF.md` current, so a fresh agent can take over
- checks usage on every agent notification, and arms, wakes and resumes as described above

The full procedure is in [`skill/SKILL.md`](skill/SKILL.md); every `lw` command and decision is in [`lw/README.md`](lw/README.md).

**Tip:** each wake re-reads the orchestrating session's whole context. For a long unattended run, start from a fresh, small session that launches the agents itself. Agents can only be resumed by the session that launched them.

## Tested behaviour

All tested on Windows 11 with the desktop app, 2026-11-15:

| Behaviour | Result |
|---|---|
| The desktop Code tab applies `env` from the user `settings.json` | Yes, in sessions started after the change |
| `CronCreate` is available in a main Code-tab session | Yes. Jobs are session-only, fire only while the session is idle, and recurring jobs expire after 7 days |
| `get_usage` reset times | ISO 8601 UTC per window (5-hour, weekly, per-model weekly) |
| A one-shot `CronCreate` wakes an idle session | Yes, three times, each within 2 s of the set minute |
| A desktop scheduled task's completion wakes an idle session | **No.** The task runs as its own session; a backup task must message the orchestrator with `send_message` |
| A scheduled task created during an automatic turn (an agent notification or timer) keeps Bypass permission mode | **No.** It runs in the default mode, so without the `permissions.allow` entries above its first `lw` call waits on a prompt |
| Dry run: wake → batch of two resumed from transcripts → next batch wake → done; the `irreversible` agent was held for a person | Yes |
| With the `permissions.allow` entries, a default-mode backup task runs `lw` and `send_message` without a prompt | Yes (the whole run took 36 s) |
| That backup task's `send_message` wakes an orchestrator running in **Bypass** mode | **No.** A Bypass session holds messages from a sender in another mode for approval, and a desktop session doesn't show that prompt. The task fell back to its toast as designed. So with a Bypass orchestrator the backup is a durable *notifier* ("work did not resume, open the session"), not a waker |
| First real use: 8 paused streams queued, woken right after a 5-hour reset, resumed in batches of two up to 4 at once, large transcripts restarted fresh from their handoff notes | Yes |

| First real 5-hour limit with the `env` block set (hit about 17:40, reset 20:30): do running agents wait instead of failing? | **Yes.** No agent failed. Two agents were mid-work at the wall, and their next reports arrived at 20:30:15, right after the reset. The orchestrator's own turn waited too |

**Not yet proven:**
- the weekly path, live
- a backup task that can wake a Bypass orchestrator. The candidate is a task saved in Bypass mode, created in a turn the user typed and re-pointed at each arm with `update_scheduled_task`, if updates keep the mode.

## Layout

- `skill/SKILL.md`: the procedure Claude follows.
- `lw/`: the Rust helper.
  - per-session ledger with atomic writes under a lock
  - reset-time parser
  - arm and wake decisions, batches and herd guard
  - transcript size check
  - toasts
- `ledger.example.json`: the ledger format, generated by `lw`.

## License

MIT, see [LICENSE](LICENSE).
