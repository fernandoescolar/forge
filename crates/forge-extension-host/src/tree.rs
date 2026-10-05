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
}

impl Default for Tree {
    fn default() -> Self {
        let root = Node { kind: NodeKind::Element { kind: "view".into(), props: Map::new(), events: vec![] }, children: vec![] };
        Self { nodes: HashMap::from([(ROOT, root)]) }
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

    /// Applies one commit. Unknown ids are logged and skipped so a single bad op can't
    /// wedge the panel.
    pub fn apply(&mut self, ops: Vec<Op>) {
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
                self.detach(child);
                self.node_mut(parent)?.children.push(child);
            }
            Op::Insert { parent, child, before } => {
                self.detach(child);
                let children = &mut self.node_mut(parent)?.children;
                let at = children.iter().position(|c| *c == before).unwrap_or(children.len());
                children.insert(at, child);
            }
            Op::Remove { parent, child } => {
                self.node_mut(parent)?.children.retain(|c| *c != child);
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
        for node in self.nodes.values_mut() {
            node.children.retain(|c| *c != child);
        }
    }

    fn drop_subtree(&mut self, id: NodeId) {
        if let Some(node) = self.nodes.remove(&id) {
            for child in node.children {
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
    fn bad_ops_are_skipped() {
        let mut t = Tree::default();
        t.apply(ops(json!([{"op":"append","parent":99,"child":1},{"op":"text","id":1,"text":"ok"},{"op":"append","parent":0,"child":1}])));
        assert_eq!(t.root().children, vec![1]);
    }
}
