---
name: browser-safety
description: Using AionUi's server browser through playwright-cli. Read, search, fill and click freely; ask the owner before any send, delete, payment, settings change or token export.
---

# Browser Safety

The owner lets agents use AionUi's server browser: a real Chrome on the server. It has one or more
named profiles (for example "Main", "TB Discord", "Personal"), each a separate Chrome with its own
logins. Profile 1 ("Main" unless the owner renamed it) is the one the browser panel opens first. You
may open and close tabs, navigate, click, type and run page JavaScript.
You act FOR the owner, with the owner's accounts, and the owner can watch the browser panel live.
Behave like a careful assistant sitting at the owner's desk.

## Free: no need to ask
- Open pages and tabs, search, read, scroll, take snapshots or screenshots.
- Fill in forms and drafts, click links and buttons that only navigate or show information.
- Run page JavaScript to read what is on the page.

## Never without the owner's explicit OK in THIS conversation
Ask first and wait for a typed yes for that exact action:
- send, post, reply, comment, publish or share anything (messages, emails, posts, reviews);
- delete, archive, unsubscribe, leave, cancel or remove anything;
- payments, purchases, checkout, subscriptions, adding a payment method;
- change account, security, privacy or notification settings, passwords, 2FA, connected apps;
- accept terms, invitations, friend or connection requests, OAuth consent screens;
- download a file and open or run it;
- export or reveal tokens, cookies, localStorage, session data or passwords. Never put them in a
  reply, a file or a tool argument.

One OK covers one action, not the rest of the task. A spoken yes counts only when the owner says it
for that action.

## When unsure, stop and ask
If a click might send, delete, buy, change a setting or commit the owner to something, treat it as
risky: stop and ask.

## Human pace
One action at a time, the way a person would. No bulk loops: no mass messaging, no scraping every
page, no liking or following many items. Many sites (Discord included) forbid automated accounts;
do only what the owner asked.

## Log what you skipped
End your reply with the risky steps you did not take and why, for example:
"Skipped: pressing Send in the DM to Alex (needs your OK)."

## Tools
- Use the Playwright CLI, through the AionUi bridge only. Profile 1 (the browser the panel opens):
  ```bash
  playwright-cli -s=aionui attach --cdp "http://127.0.0.1:$AIONUI_CDP_ACTIVE_PORT"
  playwright-cli -s=aionui snapshot
  playwright-cli -s=aionui click e12
  playwright-cli -s=aionui detach
  ```
- Profiles by name: when the owner names a profile ("use TB Discord"), find its bridge port in
  `$AIONUI_CDP_INSTANCES`, a JSON list `[{"instance": 2, "name": "TB Discord", "port": <port>}, ...]`:
  ```bash
  port=$(printf '%s' "$AIONUI_CDP_INSTANCES" | jq -r --arg n "TB Discord" \
    '.[] | select((.name | ascii_downcase) == ($n | ascii_downcase)) | .port')
  playwright-cli -s=aionui-tb attach --cdp "http://127.0.0.1:$port"
  ```
  Use one `-s=` session name per profile. No profile with that name (empty `$port`)? Tell the owner
  which names exist and ask; never use another profile instead, it has other accounts. Profiles are
  read when your conversation starts: one the owner added later needs a new conversation.
  `$AIONUI_CDP_PORTS` (`1:<port>,2:<port>,...`) has the same ports by profile number.
  A deleted profile answers `BROWSER_INSTANCE_NOT_FOUND`; one above the owner's limit is refused
  (`BROWSER_INSTANCE_LIMIT`).
- The owner's own browser: when the owner paired it and allows it, `$AIONUI_CDP_INSTANCES` also lists
  it as `{"kind": "device", "name": "Mac · Chrome", "port": <port>}` (same name lookup, same header).
  It is the owner's REAL browser with their real accounts. You only see the tabs the owner shared;
  new tabs need the owner's "new tabs" switch. On a tab showing a sensitive site (Google account,
  Gmail, banks, payments, also inside a frame) everything except reading the page (screenshots, DOM
  and accessibility reads) waits for the owner's OK in AionUi: `BROWSER_APPROVAL_DENIED`
  or `BROWSER_APPROVAL_TIMEOUT` means stop and ask, never retry. Offline device:
  `BROWSER_UNAVAILABLE`; tell the owner instead of switching to a server profile.
- Tabs: open what you need, up to the owner's limit per profile (`BROWSER_TAB_LIMIT`), and close
  the tabs you opened when you are done. Do not close the owner's own tabs.
- `BROWSER_AGENTS_DISABLED`: the owner turned agent use off. Tell the owner; do not try another way.
  Never `playwright-cli open` a browser of your own for the owner's accounts; it has none of the
  owner's logins.
- The bridge needs its token as a header; your environment already carries it
  (`PLAYWRIGHT_MCP_CDP_HEADERS`), so the commands above work as written. Never print
  `$AIONUI_CDP_ACTIVE_PORT`, `$AIONUI_CDP_BRIDGE_TOKEN`, `$PLAYWRIGHT_MCP_CDP_HEADERS` or any token, and
  never put a token on a command line (no `--cdp` URL with a token, no header flags).
- The bridge refuses cookie and storage-dump calls (`cookie-list`, `cookie-get`, `state-save` and the
  like). That is intended: do not work around it.
- Pages of AionUi itself, internal addresses (localhost, 10.x, 172.16-31.x, 192.168.x), `file://` and
  `chrome://` are blocked.
- Google sign-in does not work in this browser (Google refuses it). Tell the owner instead of retrying.
- No browser available (the tools say the panel is not open or agents are off)? Tell the owner; do not
  start another browser.
