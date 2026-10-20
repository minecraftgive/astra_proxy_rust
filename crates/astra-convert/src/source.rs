//! Source positions from parser events, independent of serde's value/merge expansion.
//! Aliases retain their use-site mark and share an arena node instead of cloning trees.
use std::collections::{HashMap, HashSet};
use yaml_rust2::{
    parser::{Event, MarkedEventReceiver, Parser},
    scanner::Marker,
};

enum Kind {
    Scalar(String),
    Sequence(Vec<usize>),
    Mapping(Vec<(usize, usize)>),
    Alias(usize),
    Unknown,
}
struct Node {
    line: usize,
    kind: Kind,
}
struct Frame {
    node: usize,
    key: Option<usize>,
}
#[derive(Default)]
pub struct Source {
    nodes: Vec<Node>,
    stack: Vec<Frame>,
    anchors: HashMap<usize, usize>,
    root: Option<usize>,
}
impl Source {
    pub fn parse(input: &str) -> Self {
        let mut source = Self::default();
        // Positions must never change which YAML inputs the converter accepts.
        if Parser::new_from_str(input)
            .load(&mut source, false)
            .is_err()
        {
            return Self::default();
        }
        source
    }
    fn add(&mut self, kind: Kind, line: usize, anchor: usize) -> usize {
        let node = self.nodes.len();
        self.nodes.push(Node { line, kind });
        if anchor != 0 {
            self.anchors.insert(anchor, node);
        }
        if let Some(parent) = self.stack.last_mut() {
            match &mut self.nodes[parent.node].kind {
                Kind::Sequence(children) => children.push(node),
                Kind::Mapping(children) => {
                    if let Some(key) = parent.key.take() {
                        children.push((key, node));
                    } else {
                        parent.key = Some(node);
                    }
                }
                _ => {}
            }
        } else if self.root.is_none() {
            self.root = Some(node);
        }
        node
    }
    fn resolve(&self, mut node: usize) -> Option<usize> {
        for _ in 0..128 {
            match self.nodes.get(node)?.kind {
                Kind::Alias(next) => node = next,
                _ => return Some(node),
            }
        }
        None
    }
    fn scalar(&self, node: usize) -> Option<&str> {
        match &self.nodes[self.resolve(node)?].kind {
            Kind::Scalar(value) => Some(value),
            _ => None,
        }
    }
    fn pair(&self, node: usize, key: &str) -> Option<(usize, usize)> {
        self.pair_inner(node, key, &mut HashSet::new())
    }
    fn pair_inner(
        &self,
        node: usize,
        key: &str,
        seen: &mut HashSet<usize>,
    ) -> Option<(usize, usize)> {
        let node = self.resolve(node)?;
        if !seen.insert(node) || seen.len() > 4096 {
            return None;
        }
        match &self.nodes[node].kind {
            Kind::Mapping(pairs) => {
                // Explicit keys override merges. Earlier maps in a merge sequence win.
                if let Some(pair) = pairs.iter().find(|(k, _)| self.scalar(*k) == Some(key)) {
                    return Some(*pair);
                }
                for (k, v) in pairs {
                    if self.scalar(*k) == Some("<<") {
                        if let Some(pair) = self.pair_inner(*v, key, seen) {
                            return Some(pair);
                        }
                    }
                }
                None
            }
            Kind::Sequence(items) => items
                .iter()
                .find_map(|node| self.pair_inner(*node, key, seen)),
            _ => None,
        }
    }
    pub fn top(&self, key: &str) -> Option<usize> {
        let (key, _) = self.pair(self.root?, key)?;
        Some(self.nodes[key].line)
    }
    pub fn item(&self, section: &str, index: usize) -> Option<usize> {
        let (_, value) = self.pair(self.root?, section)?;
        let Kind::Sequence(items) = &self.nodes[self.resolve(value)?].kind else {
            return None;
        };
        Some(self.nodes[*items.get(index)?].line)
    }
    pub fn named(&self, section: &str, name: &str) -> Option<usize> {
        let (_, value) = self.pair(self.root?, section)?;
        let (key, _) = self.pair(value, name)?;
        Some(self.nodes[key].line)
    }
}
impl MarkedEventReceiver for Source {
    fn on_event(&mut self, event: Event, mark: Marker) {
        match event {
            Event::Scalar(value, _, anchor, _) => {
                self.add(Kind::Scalar(value), mark.line(), anchor);
            }
            Event::Alias(anchor) => {
                let kind = self
                    .anchors
                    .get(&anchor)
                    .copied()
                    .map(Kind::Alias)
                    .unwrap_or(Kind::Unknown);
                self.add(kind, mark.line(), 0);
            }
            Event::SequenceStart(anchor, _) | Event::MappingStart(anchor, _) => {
                let kind = if matches!(event, Event::SequenceStart(..)) {
                    Kind::Sequence(Vec::new())
                } else {
                    Kind::Mapping(Vec::new())
                };
                let node = self.add(kind, mark.line(), anchor);
                self.stack.push(Frame { node, key: None });
            }
            Event::SequenceEnd | Event::MappingEnd => {
                self.stack.pop();
            }
            _ => {}
        }
    }
}

pub fn at(line: Option<usize>, message: impl std::fmt::Display) -> String {
    match line {
        Some(line) => format!("[第 {line} 行] {message}"),
        None => format!("[原文件行号不可用] {message}"),
    }
}
