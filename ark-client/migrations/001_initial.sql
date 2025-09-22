-- Initial schema for swap storage

CREATE TABLE swaps (
    id TEXT PRIMARY KEY,
    data TEXT NOT NULL,  -- JSON serialized SwapData
    created_at INTEGER NOT NULL,  -- Unix timestamp
    updated_at INTEGER NOT NULL   -- Unix timestamp
);

-- Index for time-based queries (optional but useful for debugging)
CREATE INDEX idx_swaps_created_at ON swaps(created_at);
CREATE INDEX idx_swaps_updated_at ON swaps(updated_at);