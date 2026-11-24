CREATE TABLE projects (
    id TEXT PRIMARY KEY NOT NULL,
    owner_id TEXT NOT NULL,
    title TEXT NOT NULL,
    is_public BOOLEAN NOT NULL,
    version BIGINT NOT NULL
);
CREATE INDEX projects_owner ON projects (owner_id);
