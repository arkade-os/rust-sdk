-- Add chain swaps table for ARK <-> BTC chain swaps via Boltz

CREATE TABLE chain_swaps (
    id TEXT PRIMARY KEY,
    data TEXT NOT NULL,  -- JSON serialized ChainSwapData
    created_at INTEGER NOT NULL,  -- Unix timestamp
    updated_at INTEGER NOT NULL   -- Unix timestamp
);

CREATE INDEX idx_chain_swaps_created_at ON chain_swaps(created_at);
CREATE INDEX idx_chain_swaps_updated_at ON chain_swaps(updated_at);
