---
name: cs-board-report
description: Board = the single list of all work. Get a card before starting, then report to your project's Board Butler when you start, reach ~50%, commit, are blocked, need a decision or finish.
---

# Report to the Board Butler

Every piece of work (feature, fix, task, research) lives on its project's board. The Butler only knows what you
tell it: report without being asked, and never wait for a reply.

## 1. Find the Butler (once per conversation)
```sh
"$AIONUI_HELPER_BIN" board status
```
→ use `keeper_conversation_id`. If it is null: `"$AIONUI_HELPER_BIN" session list` and pick the conversation
named `<project> · Board Butler` for your project.
Use the platform Butler `c7fe7b70` ONLY for work under `/root/projects/aionui/**` (incl. `.wt/` worktrees) or
`/root/projects/clauterm-studio`. No Butler found otherwise → tell the user; don't guess one.

## 2. A card BEFORE starting
Ask the Butler ("new work, no card: <title> · <why>") or create it with the `board` skill. Only a
production-breakage fix may start first (report it right after). Work found on the way → a new card.

## 3. Report as it happens
started · ~50% · a commit or fix on a branch · blocked (on what) · needs decision (which) · done (gates green).
```sh
printf '%s' '{"to":"<butler id>","message":"<card> · <started|progress N%|done|blocked: …|needs decision: …> · <evidence>"}' \
  | "$AIONUI_HELPER_BIN" session send-message
```
→ expect `"status":"delivered"`, then carry on.

## Rules
- Evidence, not adjectives: sha + branch, test + exit code, log path. No secrets. No raw `"` inside the message.
- Use the card reference when known (`TASK-87`), otherwise a short title the Butler can match.
- No bursts: one line per card per milestone; several cards changing at once → ONE message, one line per card.
- Reports are one-way: never answer a Butler acknowledgement.
- A Codex `exec` child cannot reach the helper: its parent reports for it.
