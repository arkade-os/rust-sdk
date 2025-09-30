-- Initial schema for swap storage with separate tables for submarine and reverse swaps

-- Create submarine swaps table
CREATE TABLE submarine_swaps (
    id TEXT PRIMARY KEY,
    data TEXT NOT NULL,  -- JSON serialized SubmarineSwapData
    created_at INTEGER NOT NULL,  -- Unix timestamp
    updated_at INTEGER NOT NULL   -- Unix timestamp
);

-- Create reverse swaps table
CREATE TABLE reverse_swaps (
    id TEXT PRIMARY KEY,
    data TEXT NOT NULL,  -- JSON serialized ReverseSwapData
    created_at INTEGER NOT NULL,  -- Unix timestamp
    updated_at INTEGER NOT NULL   -- Unix timestamp
);

-- Create indexes for time-based queries
CREATE INDEX idx_submarine_swaps_created_at ON submarine_swaps(created_at);
CREATE INDEX idx_submarine_swaps_updated_at ON submarine_swaps(updated_at);
CREATE INDEX idx_reverse_swaps_created_at ON reverse_swaps(created_at);
CREATE INDEX idx_reverse_swaps_updated_at ON reverse_swaps(updated_at);
