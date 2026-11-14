-- Migration 900 (youssef range 900–999): hierarchical conversations.
--
-- A conversation may be the child of another conversation (a parent spawns
-- nested sub-conversations for parallel work). Root conversations keep
-- parent_conversation_id NULL and depth 0. `depth` is denormalised so the
-- limit check and the sidebar read model never have to walk the tree; it is
-- rewritten transactionally for a whole subtree when a conversation is
-- reparented. There is deliberately no FK: deleting a parent cascades through
-- ConversationService (hooks, runtime teardown, workspace guard), and an
-- orphan (parent row missing) is read as a root.
ALTER TABLE conversations ADD COLUMN parent_conversation_id TEXT;
ALTER TABLE conversations ADD COLUMN depth INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS idx_conversations_user_parent
    ON conversations (user_id, parent_conversation_id);
