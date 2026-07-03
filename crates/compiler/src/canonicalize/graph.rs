//! Tarjan's strongly connected components, used to sort definitions into
//! dependency order (the Haskell compiler uses Data.Graph for this).
//!
//! `edges[i]` lists the nodes that node `i` depends on. Components are
//! returned with dependencies before dependents.

pub fn strongly_connected_components(count: usize, edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut state = Tarjan {
        edges,
        index: 0,
        indices: vec![None; count],
        lowlinks: vec![0; count],
        on_stack: vec![false; count],
        stack: Vec::new(),
        components: Vec::new(),
    };
    for v in 0..count {
        if state.indices[v].is_none() {
            state.visit(v);
        }
    }
    state.components
}

struct Tarjan<'a> {
    edges: &'a [Vec<usize>],
    index: usize,
    indices: Vec<Option<usize>>,
    lowlinks: Vec<usize>,
    on_stack: Vec<bool>,
    stack: Vec<usize>,
    components: Vec<Vec<usize>>,
}

impl Tarjan<'_> {
    fn visit(&mut self, v: usize) {
        self.indices[v] = Some(self.index);
        self.lowlinks[v] = self.index;
        self.index += 1;
        self.stack.push(v);
        self.on_stack[v] = true;

        for &w in &self.edges[v] {
            match self.indices[w] {
                None => {
                    self.visit(w);
                    self.lowlinks[v] = self.lowlinks[v].min(self.lowlinks[w]);
                }
                Some(w_index) => {
                    if self.on_stack[w] {
                        self.lowlinks[v] = self.lowlinks[v].min(w_index);
                    }
                }
            }
        }

        if Some(self.lowlinks[v]) == self.indices[v] {
            let mut component = Vec::new();
            loop {
                let w = self.stack.pop().unwrap();
                self.on_stack[w] = false;
                component.push(w);
                if w == v {
                    break;
                }
            }
            component.sort_unstable();
            self.components.push(component);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::strongly_connected_components;

    #[test]
    fn dependencies_come_first() {
        // 0 depends on 1, 1 depends on 2.
        let components = strongly_connected_components(3, &[vec![1], vec![2], vec![]]);
        assert_eq!(components, vec![vec![2], vec![1], vec![0]]);
    }

    #[test]
    fn cycles_are_grouped() {
        // 0 <-> 1 mutually recursive, 2 depends on both.
        let components =
            strongly_connected_components(3, &[vec![1], vec![0], vec![0, 1]]);
        assert_eq!(components, vec![vec![0, 1], vec![2]]);
    }
}
