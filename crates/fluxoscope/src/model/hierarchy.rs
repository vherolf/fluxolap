//! A dimension's elements as a hierarchy that can be shown with some
//! consolidated elements expanded (ported from fluxoterm).

use fluxolap_api_types::{ElementDto, ElementType};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default)]
pub struct Hierarchy {
    /// In position order.
    elements: Vec<ElementDto>,
    index: HashMap<u32, usize>,
}

impl Hierarchy {
    pub fn new(mut elements: Vec<ElementDto>) -> Self {
        elements.sort_by_key(|e| e.position);
        let index = elements.iter().enumerate().map(|(i, e)| (e.id, i)).collect();
        Self { elements, index }
    }

    pub fn len(&self) -> usize {
        self.elements.len()
    }

    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    pub fn get(&self, id: u32) -> Option<&ElementDto> {
        self.index.get(&id).map(|&i| &self.elements[i])
    }

    pub fn name(&self, id: u32) -> &str {
        self.get(id).map_or("?", |e| e.name.as_str())
    }

    pub fn is_consolidated(&self, id: u32) -> bool {
        self.get(id).is_some_and(|e| e.elem_type == ElementType::Consolidated)
    }

    /// Elements without a parent, in position order.
    pub fn roots(&self) -> Vec<u32> {
        self.elements.iter().filter(|e| e.parents.is_empty()).map(|e| e.id).collect()
    }

    /// Every element in position order.
    pub fn all(&self) -> Vec<u32> {
        self.elements.iter().map(|e| e.id).collect()
    }

    /// The rows to show: roots, and below each `expanded` consolidated
    /// element its children, recursively. `(id, depth)` in display order.
    pub fn visible(&self, expanded: &HashSet<u32>) -> Vec<(u32, usize)> {
        let mut out = Vec::new();
        for root in self.roots() {
            self.walk(root, 0, expanded, &mut out);
        }
        out
    }

    fn walk(&self, id: u32, depth: usize, expanded: &HashSet<u32>, out: &mut Vec<(u32, usize)>) {
        out.push((id, depth));
        if !expanded.contains(&id) {
            return;
        }
        if let Some(e) = self.get(id) {
            for child in &e.children {
                // A cycle can't exist (the server refuses them), but don't
                // recurse forever on bad data either.
                if depth < 64 {
                    self.walk(child.id, depth + 1, expanded, out);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxolap_api_types::ChildDto;

    pub fn element(id: u32, name: &str, position: u32, parents: &[u32], children: &[u32]) -> ElementDto {
        ElementDto {
            id,
            name: name.to_string(),
            elem_type: if children.is_empty() { ElementType::Numeric } else { ElementType::Consolidated },
            position,
            level: if children.is_empty() { 0 } else { 1 },
            parents: parents.to_vec(),
            children: children.iter().map(|&c| ChildDto { id: c, weight: 1.0 }).collect(),
        }
    }

    #[test]
    fn visible_rows_follow_expansion() {
        let h = Hierarchy::new(vec![element(1, "North", 1, &[0], &[]), element(0, "Total", 0, &[], &[1, 2]), element(2, "South", 2, &[0], &[])]);
        assert_eq!(h.visible(&HashSet::new()), vec![(0, 0)]);
        assert_eq!(h.visible(&HashSet::from([0])), vec![(0, 0), (1, 1), (2, 1)]);
        assert_eq!(h.all(), vec![0, 1, 2]);
        assert!(h.is_consolidated(0) && !h.is_consolidated(1));
    }
}
