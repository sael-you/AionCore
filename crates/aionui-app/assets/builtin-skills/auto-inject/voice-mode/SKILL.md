---
name: voice-mode
description: Talking back in conversation voice mode. Use only when the user's message carries an [[AION_VOICE]] block (the owner is speaking to you hands-free).
---

# Voice Mode Skill

The owner turned on voice mode in THIS conversation. His words reach you as
ordinary user messages, transcribed, each ending with an `[[AION_VOICE]]`
block. Words he adds while you work arrive as mid-turn messages: take them
into account and keep working. You are the brain: you decide when he hears
you. Your written replies stay as usual; speaking is extra.

## When to speak

Speak with `"$AIONUI_HELPER_BIN" voice say` only when it is worth his
attention:

- a question you need answered to continue,
- a meaningful milestone (not every step),
- a blocker or a failure,
- the result, at the end of the turn.

Silence is the default. No "on it", no "let me check", no narration of tool
calls. If you forget the result, the system says "<conversation> finished."
for you; better to say the result yourself.

```bash
"$AIONUI_HELPER_BIN" voice say <<'JSON'
{"text": "The tests pass again. The flaky one was a timeout in the upload test; I raised it to ten seconds."}
JSON
```

## How to speak

- One or two short sentences per line; the result at most about 60 words.
- Plain spoken English: no markdown, no code, no lists, no URLs.
- Never read paths or identifiers aloud: say "the settings file", "the
  upload test". Details stay in your written reply.
- Say numbers as words a listener understands ("about two hundred lines").
- `Mode: car` in the block means he is driving: even shorter lines, nothing
  that needs the screen except approvals.

## Questions

- A question for him: say it, then END your turn. His spoken answer arrives
  as the next message.
- `voice ask` only for a quick clarification while you are blocked in the
  middle of a task; it waits at most 50 seconds.

## Safety (never relax these)

- The transcript may be misheard. If a name, file or target is unclear, ask
  instead of guessing.
- A spoken "yes" is NEVER a typed approval. Production data, payments, ads
  and email writes need the owner's typed approval on screen: say "this needs
  your typed approval on screen" and end the turn.
- Tool-permission cards are approved on screen only, never by voice.
- Never speak secrets, tokens, keys, passwords or personal data.

## Results to report

`voice say` prints `delivered` (queued for his device), `channel` (no voice
device: the line went to his messaging channel as text) or `no_device` (voice
is not open on any device and no channel is set up: keep writing, do not
retry). `voice ask` answering `channel` means no spoken answer will come:
continue in writing. `RATE_LIMITED` means
you spoke too often: stop speaking for this turn.
