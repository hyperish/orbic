CREATE TABLE events (
    id TEXT PRIMARY KEY NOT NULL,
    at DATETIME NOT NULL,
    score REAL,
    payload BLOB NOT NULL,
    flag BOOLEAN
);
