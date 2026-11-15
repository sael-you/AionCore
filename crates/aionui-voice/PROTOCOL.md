# Voice Concierge protocol (card C9)

The single shared contract between AionCore (`aionui-voice`) and the AionUi
Voice mode (S2). Source of the design: `clauterm-studio/design/voice/PLAN.md`
v15 (§3.5 auth, §3.6 lifecycle, §3.9 frames). Types live in
`aionui-api-types/src/voice.rs`.

Status legend: **S1a** = implemented in this crate now; **S1b+** = agreed
shape, implemented in a later slice.

## Conventions

- REST bodies are **snake_case** (like most core DTOs) and wrapped in the
  usual `{success, data}` / `{success: false, error, code}` envelopes.
- `/api/voice/ws` frames use the core envelope `{"name": "voice.camelCaseAction", "data": {...}}`.
  Frame `data` is **camelCase** and carries `voiceSessionId` (may be `null`
  while armed without a call).
- Money is USD (`*_usd`); the backend meters in integer micro-USD.

## Authentication and request rules (all voice routes, S1a)

| Rule | Behaviour |
|---|---|
| Identity | Session cookie `aionui-session` only, validated like `auth_middleware` in UserSession mode — also when AionCore runs `--local` (the local default user is never accepted) → `401 UNAUTHORIZED` |
| Credential precedence | Requests carrying `Authorization`, `x-aionui-runtime-token`, `x-aionui-user-id`, `x-aionui-conversation-id` or `Sec-WebSocket-Protocol` → `400 VOICE_CREDENTIAL_CONFLICT` |
| CSRF | Unsafe methods need `x-csrf-token` = the `aionui-csrf-token` cookie → else `403 CSRF_INVALID` (voice adds the layer in local mode; other modes use the global one) |
| Origin | `Origin` must be in `AIONUI_VOICE_ALLOWED_ORIGINS` (comma-separated, exact `scheme://host[:port]`) → `403 VOICE_ORIGIN_DENIED`; unsafe methods and WS upgrades without `Origin` → `403 VOICE_ORIGIN_REQUIRED`; same-origin GETs without `Origin` are allowed |
| Rate limit | `POST /api/voice/sessions` and `…/end`: 20/min per user → `429 RATE_LIMITED` |
| Re-validation | Live sockets and calls re-check the cookie every 60 s; a revoked session (logout everywhere, password change) closes the socket (`voice.error AUTH_EXPIRED`) and hangs up the call (`voice.callEnded reason=auth_revoked`) |

## REST (S1a)

| Route | Body | Response `data` | Errors |
|---|---|---|---|
| `GET /api/voice/settings` | — | `{enabled, engine, wake_word_enabled, car_mode, daily_budget_usd, used_today_usd, model, voice, eagerness, provider_id?}` | |
| `PUT /api/voice/settings` | any subset of `{enabled, engine, wake_word_enabled, car_mode, daily_budget_usd, model, voice, eagerness, provider_id}` (`used_today_usd` accepted and ignored; unknown fields → 400) | same as GET | `400 BAD_REQUEST` |
| `GET /api/voice/status` | — | `{enabled, live_calls, active_session_id?, used_today_usd, limit_usd}` | |
| `POST /api/voice/sessions` | `{engine?, sdp_offer, device_id?, focus_conversation_id?, resume_of?}` | `201 {voice_session_id, sdp_answer, engine, model}` | `403 VOICE_DISABLED`, `409 VOICE_BUSY` (live call on another device), `412 VOICE_NO_PROVIDER`, `429 VOICE_BUDGET_REACHED`, `400 BAD_REQUEST`, `502 BAD_GATEWAY` (provider refused) |
| `GET /api/voice/sessions` | — | last 50 sessions `[{id, engine, model, state, end_reason?, created_at, ready_at?, ended_at?, input_tokens, cached_input_tokens, output_tokens, responses, unknown_responses, cost_usd}]` | |
| `POST /api/voice/sessions/{id}/end` | optional `{reason}`: `user` \| `sleep` \| `idle` \| `offline` \| `network` (others → `user`) | the session | `404 NOT_FOUND` (not yours / unknown) |
| `GET /api/voice/readbacks/{clipId}` | — | audio clip (same-origin path only) | **S1b** |

Notes:
- Voice is **off by default**. `PUT {enabled: false}` is the kill switch: live
  calls are hung up (`voice.callEnded reason=disabled`), every voice socket of
  the user gets `voice.error VOICE_DISABLED` and is closed, new sessions → 403.
- `focus_conversation_id` is accepted and validated; it is used from S1b.
- `resume_of` ends that call first (`reason=resumed`), then admits the new one.

## `/api/voice/ws`

Upgrade with the session cookie + allowed `Origin`, no subprotocol. Limits:
control frames ≤ 64 KB (larger → socket closed) and ≤ 10/s
(`voice.error RATE_LIMITED`); audio frames (S3) ≤ 32 KB and ≤ 50/s. The first
frame must be `voice.hello` within 5 s (`HELLO_REQUIRED`, then close).
After 20 limit violations the socket is closed.

Every frame naming a `voiceSessionId` that is not one of the user's sessions is
refused with `voice.error SESSION_MISMATCH` (frame dropped).

### Client → server

| Frame | `data` | Status |
|---|---|---|
| `voice.hello` | `{voiceSessionId?, device, capabilities}` — send on connect, again right after `POST /api/voice/sessions`, and on every reconnect, to bind the socket | S1a |
| `voice.heartbeat` | `{voiceSessionId?, t}` every 10 s; no heartbeat for 25 s while a call is live → the call is hung up (`device_stale`) | S1a |
| `voice.frozen` / `voice.resumed` | `{voiceSessionId?}` page lifecycle; `frozen` hangs up a live call (`device_frozen`) | S1a |
| `voice.playbackFinished` | `{voiceSessionId, readbackId, tEnd}` | S1b (S1a answers `UNSUPPORTED`) |
| `voice.speechOnset` | `{voiceSessionId, t}` | S1b |
| `voice.confirmTap` / `voice.cancelTap` | `{voiceSessionId, draftId, version, hash}` | S1b |

### Server → client

| Frame | `data` | Status |
|---|---|---|
| `voice.ready` | `{voiceSessionId, engine, model}` — config acknowledged by the provider; the client may unmute. Also re-sent after a sideband re-attach and on `hello`/`resumed` for a ready call | S1a |
| `voice.callEnded` | `{voiceSessionId, reason}` — **sent exactly once when a call ends, for any reason. The client ends its call only on this frame.** | S1a |
| `voice.state` | `{voiceSessionId, state, reason?}` — **display only** (see below) | S1a |
| `voice.caption` | `{voiceSessionId, role: user\|assistant, text, final}` | S1a |
| `voice.assistantAudioFinished` | `{voiceSessionId, responseId}` — provider finished sending audio (play "your turn" after the measured output latency) | S1a |
| `voice.budget` | `{voiceSessionId, usedUsd, limitUsd, warning?: "80"\|"100"}` | S1a |
| `voice.error` | `{voiceSessionId, code, message}`; codes containing `DISABLED` (e.g. `VOICE_DISABLED`) mean voice was turned off → disarm | S1a |
| `voice.draftUpdated` | `{voiceSessionId, draftId, version, hash, state, target: {id, name, kind?}, text, diff: [{op: equal\|insert\|delete, text, transform?: filler\|reference\|reorder}], delivery: new_turn\|steer\|queue_busy\|queue_approval\|new_conversation, holdRemainingMs?, holdFrozen?}` — `holdRemainingMs` = ms left in the 4 s hold (§3.3.5), `holdFrozen` = true while speech froze the deadline; the UI falls back to a local 4 s countdown when absent | S1b |
| `voice.readbackReady` | `{voiceSessionId, draftId, version, readbackId, url}` (`url` = `/api/voice/readbacks/{id}`) | S1b |
| `voice.wake` | `{voiceSessionId, reason, itemId}` | S1b |
| `voice.followUp` | `{voiceSessionId, conversationId, status, line, clipId}` (`clipId` fetched from `/api/voice/readbacks/{clipId}`) | S1b |

### `voice.state` semantics

| `state` | `reason` | Meaning |
|---|---|---|
| `armed` | none | socket bound, no ready call (e.g. `hello` before the call is ready) — **not** an end |
| `armed` | `resumed` | page resumed, no ready call |
| `armed` / `offline` | an end reason | follows `voice.callEnded` (display) |
| `listening` / `thinking` / `speaking` | none | call activity (user speech, waiting, assistant audio) |
| `offline` | `reconnecting` | sideband lost, backend re-attaching: stay muted until `voice.ready`; a failed re-attach ends with `voice.callEnded reason=control_lost` |

### End reasons (`voice.callEnded.reason`, `voice_sessions.end_reason`)

`user`, `sleep`, `offline`, `network` (client-requested) · `idle` (3 min
silence) · `hard_cap` (55 min) · `rotation` (≥ 30 min at an idle point) ·
`budget` (daily cap) · `disabled` (kill switch) · `auth_revoked` · `device_frozen`
· `device_stale` · `resumed` (replaced by `resume_of`) · `config_timeout` (no
`session.updated` in 5 s) · `attach_failed` · `control_lost` (re-attach failed
within 20 s) · `provider_ended` · `restart` (could not recover after a backend
restart). Setup/transport failures are stored with state `failed`, the rest
`ended`.
