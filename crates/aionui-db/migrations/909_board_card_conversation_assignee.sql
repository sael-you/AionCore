-- 909 · Board cards assigned to a conversation (TASK-116, D42 registry).
--
-- What: `board_cards.assignee_conversation_id` — the conversation a card is
-- assigned to (`assignee_kind = conversation` on the wire). 901's CHECK on
-- `assignee_kind` only admits assistant / team / user, so the conversation
-- assignee has its own nullable column instead of rebuilding the table.
-- Why: the Board Butler/Keeper assigns cards to the conversations doing them
-- (`aioncore board update <N> --assignee-conversation <id>`).
--
-- Additive only: one nullable column, no data rewritten.
-- Rollback: there are no down migrations; an older binary refuses a DB that
-- holds 909, so a rollback restores the pre-deploy DB backup. Lost: only the
-- conversation assignees set after the deploy (the cards themselves stay; the
-- `card_updated` events still record the change).
--
-- Frozen once deployed: never edit or renumber this file.

ALTER TABLE board_cards ADD COLUMN assignee_conversation_id TEXT;
