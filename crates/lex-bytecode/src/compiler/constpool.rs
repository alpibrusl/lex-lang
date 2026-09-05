//! Constant pool and record-shape interning shared by every function
//! compiled from one program: constants, field / variant / NodeId name
//! slots, and the deduplicated field-index vectors that name record
//! shapes. Owned by `compile_program` (see `super`) and threaded
//! through each `FnCompiler`.

use crate::op::*;
use indexmap::IndexMap;

#[derive(Default)]
pub(super) struct ConstPool {
    pub(super) pool: Vec<Const>,
    pub(super) fields: IndexMap<String, u32>,
    pub(super) variants: IndexMap<String, u32>,
    pub(super) node_ids: IndexMap<String, u32>,
    pub(super) ints: IndexMap<i64, u32>,
    pub(super) bools: IndexMap<u8, u32>,
    pub(super) strs: IndexMap<String, u32>,
    /// Interned record field-name shapes (#461). Deduplicated by content
    /// so a record literal with the same field-name layout reuses the
    /// same `shape_idx` across the whole program — keeps the side-table
    /// small even when the same struct is constructed in many places.
    pub(super) record_shapes: Vec<Vec<u32>>,
    pub(super) record_shape_dedup: IndexMap<Vec<u32>, u32>,
}

impl ConstPool {
    pub(super) fn field(&mut self, name: &str) -> u32 {
        if let Some(i) = self.fields.get(name) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::FieldName(name.into()));
        self.fields.insert(name.into(), i);
        i
    }
    pub(super) fn variant(&mut self, name: &str) -> u32 {
        if let Some(i) = self.variants.get(name) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::VariantName(name.into()));
        self.variants.insert(name.into(), i);
        i
    }
    pub(super) fn node_id(&mut self, name: &str) -> u32 {
        if let Some(i) = self.node_ids.get(name) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::NodeId(name.into()));
        self.node_ids.insert(name.into(), i);
        i
    }
    pub(super) fn int(&mut self, n: i64) -> u32 {
        if let Some(i) = self.ints.get(&n) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::Int(n));
        self.ints.insert(n, i);
        i
    }
    pub(super) fn bool(&mut self, b: bool) -> u32 {
        let key = b as u8;
        if let Some(i) = self.bools.get(&key) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::Bool(b));
        self.bools.insert(key, i);
        i
    }
    pub(super) fn str(&mut self, s: &str) -> u32 {
        if let Some(i) = self.strs.get(s) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::Str(s.into()));
        self.strs.insert(s.into(), i);
        i
    }
    pub(super) fn float(&mut self, f: f64) -> u32 {
        // Floats: not deduped (NaN issues).
        let i = self.pool.len() as u32;
        self.pool.push(Const::Float(f));
        i
    }
    pub(super) fn unit(&mut self) -> u32 {
        let i = self.pool.len() as u32;
        self.pool.push(Const::Unit);
        i
    }

    /// Intern a record field-name shape (#461). Returns the index into
    /// `record_shapes`; identical shapes (same field-name-index vec)
    /// always return the same index.
    pub(super) fn record_shape(&mut self, idxs: Vec<u32>) -> u32 {
        if let Some(i) = self.record_shape_dedup.get(&idxs) {
            return *i;
        }
        let i = self.record_shapes.len() as u32;
        self.record_shape_dedup.insert(idxs.clone(), i);
        self.record_shapes.push(idxs);
        i
    }
}
