//! Trace diff: walk two trace trees in parallel and report the first
//! NodeId where outputs differ. Spec §10.3.

use crate::recorder::{TraceNode, TraceTree};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Divergence {
    /// First NodeId where the two traces diverge.
    pub node_id: String,
    /// Output (or error) on side A.
    pub a: Side,
    /// Output (or error) on side B.
    pub b: Side,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Side {
    Output { value: serde_json::Value },
    Error { message: String },
    Missing,
}

pub fn diff_runs(a: &TraceTree, b: &TraceTree) -> Option<Divergence> {
    walk_pair(&a.nodes, &b.nodes)
}

fn walk_pair(a: &[TraceNode], b: &[TraceNode]) -> Option<Divergence> {
    for i in 0..std::cmp::max(a.len(), b.len()) {
        match (a.get(i), b.get(i)) {
            (Some(na), Some(nb)) => {
                if na.node_id != nb.node_id {
                    return Some(Divergence {
                        node_id: na.node_id.clone(),
                        a: side_of(na),
                        b: side_of(nb),
                    });
                }
                if na.input != nb.input || na.output != nb.output || na.error != nb.error {
                    return Some(Divergence {
                        node_id: na.node_id.clone(),
                        a: side_of(na),
                        b: side_of(nb),
                    });
                }
                if let Some(d) = walk_pair(&na.children, &nb.children) {
                    return Some(d);
                }
            }
            (Some(na), None) => return Some(Divergence {
                node_id: na.node_id.clone(),
                a: side_of(na),
                b: Side::Missing,
            }),
            (None, Some(nb)) => return Some(Divergence {
                node_id: nb.node_id.clone(),
                a: Side::Missing,
                b: side_of(nb),
            }),
            (None, None) => break,
        }
    }
    None
}

fn side_of(n: &TraceNode) -> Side {
    if let Some(e) = &n.error {
        Side::Error { message: e.clone() }
    } else if let Some(o) = &n.output {
        Side::Output { value: o.clone() }
    } else {
        Side::Missing
    }
}
