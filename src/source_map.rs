use tower_lsp::lsp_types::{Position, Range};

/// Manages bidirectional coordinate mapping between `.rs.html` template files
/// and the transpiled virtual Rust file wrapped in `__rshtml_virtual_context(&self)`.
#[derive(Debug, Clone)]
pub struct SourceMap {
    /// Number of lines inserted in the virtual header before template body starts.
    pub header_lines_count: usize,
    /// Indentation spaces prepended to each line of the template body inside the function.
    pub indent_spaces: usize,
}

impl SourceMap {
    /// Creates a new source map with known header line count and indentation.
    pub fn new(header_lines_count: usize, indent_spaces: usize) -> Self {
        Self {
            header_lines_count,
            indent_spaces,
        }
    }

    /// Converts a cursor position from `.rs.html` template coordinates to virtual Rust coordinates.
    pub fn template_to_virtual(&self, template_pos: Position) -> Position {
        Position {
            line: template_pos.line + self.header_lines_count as u32,
            character: template_pos.character + self.indent_spaces as u32,
        }
    }

    /// Converts a position from virtual Rust coordinates back to `.rs.html` template coordinates.
    /// Returns `None` if the position falls inside the generated header preamble.
    pub fn virtual_to_template(&self, virtual_pos: Position) -> Option<Position> {
        if (virtual_pos.line as usize) < self.header_lines_count {
            return None;
        }

        let mapped_line = virtual_pos.line - self.header_lines_count as u32;
        let mapped_char = virtual_pos.character.saturating_sub(self.indent_spaces as u32);

        Some(Position {
            line: mapped_line,
            character: mapped_char,
        })
    }

    /// Converts a virtual Rust range back to template range.
    pub fn virtual_to_template_range(&self, virtual_range: Range) -> Option<Range> {
        let start = self.virtual_to_template(virtual_range.start)?;
        let end = self.virtual_to_template(virtual_range.end)?;
        Some(Range { start, end })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_template_to_virtual_mapping() {
        let sm = SourceMap::new(10, 8);

        let template_pos = Position::new(5, 12);
        let virt_pos = sm.template_to_virtual(template_pos);

        assert_eq!(virt_pos.line, 15);
        assert_eq!(virt_pos.character, 20);
    }

    #[test]
    fn test_virtual_to_template_mapping() {
        let sm = SourceMap::new(10, 8);

        // Position inside template body
        let virt_pos = Position::new(15, 20);
        let template_pos = sm.virtual_to_template(virt_pos).expect("Should map back");

        assert_eq!(template_pos.line, 5);
        assert_eq!(template_pos.character, 12);

        // Position in header (line < 10) must return None
        let header_pos = Position::new(3, 4);
        assert!(sm.virtual_to_template(header_pos).is_none());
    }

    #[test]
    fn test_range_conversion() {
        let sm = SourceMap::new(10, 8);

        let virt_range = Range {
            start: Position::new(12, 10),
            end: Position::new(12, 25),
        };

        let template_range = sm.virtual_to_template_range(virt_range).expect("Must map range");
        assert_eq!(template_range.start, Position::new(2, 2));
        assert_eq!(template_range.end, Position::new(2, 17));
    }
}
