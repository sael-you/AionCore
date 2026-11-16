-- Migration 908 (youssef range 900–999): project kits (card C28, TASK-108).
-- Frozen once deployed: never edit or renumber; a change is a new 9NN file.
--
-- A kit with `project_id` set is the PROJECT KIT of that project
-- (`projects.project_id`): the resolver treats it as an implicit `project`
-- level `add` attachment of its own project (no attachment row), and it never
-- applies in another project. At most one live project kit per (user,
-- project). NULL = a shared kit (C27 behaviour, unchanged).
--
-- The K0 "Global" seed and the one-time move of builtin kits off the global
-- level are data steps in the kit service (guarded by a `client_preferences`
-- marker), not part of this migration.
--
-- Rollback: additive only (one nullable column + two indexes). An older
-- binary refuses a DB holding 908, so a rollback restores the pre-deploy
-- backup; lost = kits/attachments written since the deploy, nothing else.

ALTER TABLE kits ADD COLUMN project_id TEXT;

CREATE INDEX IF NOT EXISTS idx_kits_user_project ON kits (user_id, project_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_kits_one_project_kit
    ON kits (user_id, project_id) WHERE project_id IS NOT NULL AND deleted_at IS NULL;
