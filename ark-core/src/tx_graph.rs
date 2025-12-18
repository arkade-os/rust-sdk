use crate::Error;
use crate::VTXO_INPUT_INDEX;
use bitcoin::taproot::Signature;
use bitcoin::Psbt;
use bitcoin::Txid;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct TxGraph {
    root: Psbt,
    children: HashMap<u32, TxGraph>,
}

#[derive(Debug, Clone)]
pub struct TxGraphChunk {
    pub txid: Option<Txid>,
    pub tx: Psbt,
    pub children: HashMap<u32, Txid>,
}

impl TxGraph {
    pub fn new(chunks: Vec<TxGraphChunk>) -> Result<Self, Error> {
        if chunks.is_empty() {
            return Err(Error::ad_hoc("empty chunks"));
        }

        // Create a map to store all chunks by their txid for easy lookup.
        let mut chunks_by_txid = HashMap::new();

        for chunk in chunks {
            let txid = chunk.tx.unsigned_tx.compute_txid();
            chunks_by_txid.insert(txid, chunk);
        }

        // Find the root chunks (the ones that aren't referenced as a child).
        let mut root_txids = Vec::new();

        for txid in chunks_by_txid.keys() {
            let mut is_child = false;

            for (other_txid, other_chunk) in &chunks_by_txid {
                if other_txid == txid {
                    // Skip self.
                    continue;
                }

                // Check if the current chunk is a child of the other chunk.
                if other_chunk
                    .children
                    .values()
                    .any(|child_txid| child_txid == txid)
                {
                    is_child = true;
                    break;
                }
            }

            // If the chunk is not a child of any other chunk, it is a root.
            if !is_child {
                root_txids.push(*txid);
            }
        }

        if root_txids.is_empty() {
            return Err(Error::ad_hoc("no root chunk found"));
        }

        if root_txids.len() > 1 {
            return Err(Error::ad_hoc(format!(
                "multiple root chunks found: {root_txids:?}",
            )));
        }

        let graph = Self::build_graph(root_txids[0], &chunks_by_txid).ok_or_else(|| {
            Error::ad_hoc(format!("chunk not found for root txid: {}", root_txids[0]))
        })?;

        // Verify that the number of chunks is equal to the number of nodes in the graph
        let chunk_count = chunks_by_txid.len();
        let node_count = graph.nb_of_nodes();
        if node_count != chunk_count {
            return Err(Error::ad_hoc(format!(
                "number of chunks ({chunk_count}) is not equal to \
                 the number of nodes in the graph ({node_count})",
            )));
        }

        Ok(graph)
    }

    fn build_graph(root_txid: Txid, chunks_by_txid: &HashMap<Txid, TxGraphChunk>) -> Option<Self> {
        let root_chunk = chunks_by_txid.get(&root_txid)?;

        let mut children = HashMap::new();

        for (output_index, child_txid) in &root_chunk.children {
            if let Some(child_graph) = Self::build_graph(*child_txid, chunks_by_txid) {
                children.insert(*output_index, child_graph);
            }
        }

        Some(TxGraph {
            root: root_chunk.tx.clone(),
            children,
        })
    }

    pub fn nb_of_nodes(&self) -> usize {
        let mut nb = 1;
        for child in self.children.values() {
            nb += child.nb_of_nodes();
        }
        nb
    }

    pub fn apply<F>(&mut self, f: F) -> Result<(), Error>
    where
        F: Fn(&mut TxGraph) -> Result<bool, Error> + Copy,
    {
        let should_continue = f(self)?;

        if !should_continue {
            return Ok(());
        }

        for child in self.children.values_mut() {
            child.apply(f)?;
        }

        Ok(())
    }

    pub fn find(&self, txid: &Txid) -> Option<&Self> {
        if self.root.unsigned_tx.compute_txid() == *txid {
            return Some(self);
        }

        for child in self.children.values() {
            if let Some(node) = child.find(txid) {
                return Some(node);
            }
        }

        None
    }

    pub fn as_map(&self) -> HashMap<Txid, &Psbt> {
        fn _as_map<'a>(graph: &'a TxGraph, map: &mut HashMap<Txid, &'a Psbt>) {
            map.insert(graph.root.unsigned_tx.compute_txid(), &graph.root);

            for (_, child) in graph.children.iter() {
                _as_map(child, map);
            }
        }

        let mut map = HashMap::new();
        _as_map(self, &mut map);

        map
    }

    pub fn set_signature(&mut self, sig: Signature) {
        self.root.inputs[VTXO_INPUT_INDEX].tap_key_sig = Some(sig);
    }

    pub fn root(&self) -> &Psbt {
        &self.root
    }

    /// Return all leaf nodes (transactions without children) in the graph.
    pub fn leaves(&self) -> Vec<&Psbt> {
        if self.children.is_empty() {
            return vec![&self.root];
        }

        let mut leaves = Vec::new();
        for child in self.children.values() {
            leaves.extend(child.leaves());
        }

        leaves
    }
}
