//! The UI tree of one extension panel, mutated by ops from the JS reconciler.

use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;

pub type NodeId = u32;
pub const ROOT: NodeId = 0;

/// Mirrors `Op` in packages/forge-api/src/native.ts.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Op {
    Create { id: NodeId, #[serde(rename = "type")] kind: String, props: Map<String, Value>, events: Vec<String> },
    Text { id: NodeId, text: String },
    Append { parent: NodeId, child: NodeId },
    Insert { parent: NodeId, child: NodeId, before: NodeId },
    Remove { parent: NodeId, child: NodeId },
    Update { id: NodeId, props: Map<String, Value>, events: Vec<String> },
    SetText { id: NodeId, text: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind {
    Element { kind: String, props: Map<String, Value>, events: Vec<String> },
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub kind: NodeKind,
    pub children: Vec<NodeId>,
}

impl Node {
    pub fn prop(&self, key: &str) -> Option<&Value> {
        match &self.kind {
            NodeKind::Element { props, .. } => props.get(key),
            NodeKind::Text(_) => None,
        }
    }
    pub fn str_prop(&self, key: &str) -> Option<&str> {
        self.prop(key).and_then(Value::as_str)
    }
    pub fn has_event(&self, name: &str) -> bool {
        matches!(&self.kind, NodeKind::Element { events, .. } if events.iter().any(|e| e == name))
    }
}

#[derive(Debug)]
pub struct Tree {
    nodes: HashMap<NodeId, Node>,
    /// Each attached node's parent: moving a node takes it out of that one only (looking
    /// through every node made mounting a long list quadratic).
    parents: HashMap<NodeId, NodeId>,
    /// Counts commits: views redo what they derive from the tree when it changes.
    generation: u64,
}

impl Default for Tree {
    fn default() -> Self {
        let root = Node { kind: NodeKind::Element { kind: "view".into(), props: Map::new(), events: vec![] }, children: vec![] };
        Self { nodes: HashMap::from([(ROOT, root)]), parents: HashMap::new(), generation: 0 }
    }
}

impl Tree {
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }

    pub fn root(&self) -> &Node {
        &self.nodes[&ROOT]
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Applies one commit. Unknown ids are logged and skipped so a single bad op can't
    /// wedge the panel.
    pub fn apply(&mut self, ops: Vec<Op>) {
        self.generation += 1;
        for op in ops {
            if let Err(e) = self.apply_one(op) {
                log::warn!("extension UI op ignored: {e}");
            }
        }
    }

    fn apply_one(&mut self, op: Op) -> Result<(), String> {
        match op {
            Op::Create { id, kind, props, events } => {
                self.nodes.insert(id, Node { kind: NodeKind::Element { kind, props, events }, children: vec![] });
            }
            Op::Text { id, text } => {
                self.nodes.insert(id, Node { kind: NodeKind::Text(text), children: vec![] });
            }
            Op::Append { parent, child } => {
                self.node_mut(parent)?;
                self.detach(child);
                self.node_mut(parent)?.children.push(child);
                self.parents.insert(child, parent);
            }
            Op::Insert { parent, child, before } => {
                self.node_mut(parent)?;
                self.detach(child);
                let children = &mut self.node_mut(parent)?.children;
                let at = children.iter().position(|c| *c == before).unwrap_or(children.len());
                children.insert(at, child);
                self.parents.insert(child, parent);
            }
            Op::Remove { parent, child } => {
                self.node_mut(parent)?.children.retain(|c| *c != child);
                if self.parents.get(&child) == Some(&parent) {
                    self.parents.remove(&child);
                }
                self.drop_subtree(child);
            }
            Op::Update { id, props: new_props, events: new_events } => match &mut self.node_mut(id)?.kind {
                NodeKind::Element { props, events, .. } => {
                    *props = new_props;
                    *events = new_events;
                }
                NodeKind::Text(_) => return Err(format!("update on text node {id}")),
            },
            Op::SetText { id, text } => match &mut self.node_mut(id)?.kind {
                NodeKind::Text(t) => *t = text,
                NodeKind::Element { .. } => return Err(format!("setText on element {id}")),
            },
        }
        Ok(())
    }

    fn node_mut(&mut self, id: NodeId) -> Result<&mut Node, String> {
        self.nodes.get_mut(&id).ok_or_else(|| format!("unknown node {id}"))
    }

    /// React may move a node by appending it elsewhere without removing it first.
    fn detach(&mut self, child: NodeId) {
        if let Some(parent) = self.parents.remove(&child)
            && let Some(node) = self.nodes.get_mut(&parent)
        {
            node.children.retain(|c| *c != child);
        }
    }

    fn drop_subtree(&mut self, id: NodeId) {
        if let Some(node) = self.nodes.remove(&id) {
            for child in node.children {
                self.parents.remove(&child);
                self.drop_subtree(child);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ops(v: Value) -> Vec<Op> {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn builds_updates_and_removes() {
        let mut t = Tree::default();
        t.apply(ops(json!([
            {"op":"create","id":1,"type":"view","props":{},"events":[]},
            {"op":"create","id":2,"type":"button","props":{"label":"a"},"events":["onClick"]},
            {"op":"text","id":3,"text":"hi"},
            {"op":"append","parent":1,"child":2},
            {"op":"insert","parent":1,"child":3,"before":2},
            {"op":"append","parent":0,"child":1}
        ])));
        assert_eq!(t.root().children, vec![1]);
        assert_eq!(t.get(1).unwrap().children, vec![3, 2]);
        assert!(t.get(2).unwrap().has_event("onClick"));

        t.apply(ops(json!([{"op":"update","id":2,"props":{"label":"b"},"events":[]},{"op":"setText","id":3,"text":"yo"}])));
        assert_eq!(t.get(2).unwrap().str_prop("label"), Some("b"));
        assert!(!t.get(2).unwrap().has_event("onClick"));
        assert_eq!(t.get(3).unwrap().kind, NodeKind::Text("yo".into()));

        t.apply(ops(json!([{"op":"remove","parent":0,"child":1}])));
        assert!(t.root().children.is_empty());
        assert_eq!(t.len(), 1, "subtree dropped");
    }

    #[test]
    fn moving_a_node_takes_it_out_of_its_old_parent() {
        let mut t = Tree::default();
        t.apply(ops(json!([
            {"op":"create","id":1,"type":"view","props":{},"events":[]},
            {"op":"create","id":2,"type":"view","props":{},"events":[]},
            {"op":"text","id":3,"text":"x"},
            {"op":"append","parent":0,"child":1},
            {"op":"append","parent":0,"child":2},
            {"op":"append","parent":1,"child":3},
            {"op":"append","parent":2,"child":3}
        ])));
        assert!(t.get(1).unwrap().children.is_empty());
        assert_eq!(t.get(2).unwrap().children, vec![3]);
        t.apply(ops(json!([{"op":"insert","parent":1,"child":3,"before":99}])));
        assert_eq!((t.get(1).unwrap().children.clone(), t.get(2).unwrap().children.clone()), (vec![3], vec![]));
    }

    /// Mounting a long list stays linear.
    #[test]
    fn mounts_long_lists_quickly() {
        let mut t = Tree::default();
        let mut list = vec![json!({"op":"create","id":1,"type":"view","props":{},"events":[]}), json!({"op":"append","parent":0,"child":1})];
        for id in 2..20_002u32 {
            list.push(json!({"op":"text","id":id,"text":"row"}));
            list.push(json!({"op":"append","parent":1,"child":id}));
        }
        let started = std::time::Instant::now();
        t.apply(ops(Value::Array(list)));
        assert_eq!(t.get(1).unwrap().children.len(), 20_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
    }

    #[test]
    fn bad_ops_are_skipped() {
        let mut t = Tree::default();
        t.apply(ops(json!([{"op":"append","parent":99,"child":1},{"op":"text","id":1,"text":"ok"},{"op":"append","parent":0,"child":1}])));
        assert_eq!(t.root().children, vec![1]);
    }
}
