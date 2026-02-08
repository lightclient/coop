use crate::engine::{Component, StyledLine};

/// Spacer component that renders empty lines.
/// Direct translation of pi's spacer.js.
#[derive(Debug)]
pub struct Spacer {
    lines: usize,
}

impl Spacer {
    pub fn new(lines: usize) -> Self {
        Self { lines }
    }
}

impl Component for Spacer {
    fn render(&self, _width: usize) -> Vec<StyledLine> {
        vec![String::new(); self.lines]
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spacer_renders_empty_lines() {
        let s = Spacer::new(3);
        let lines = s.render(80);
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(line.is_empty());
        }
    }

    #[test]
    fn spacer_zero() {
        let s = Spacer::new(0);
        assert!(s.render(80).is_empty());
    }
}
