use tower_lsp::lsp_types::{Position, Range};
use tree_sitter::Tree;

/// The semantic classification of a mapped code segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    /// Pure Rust block, e.g. `@{ let x = 10; }` or `@if condition { ... }`
    RustBlock,
    /// Inline expression starting with `@`, e.g. `@self.title` or `@self.calc()`
    /// (the template span includes `@`, while the virtual span excludes `@`)
    AtExpression,
    /// Expression inside a component property attribute, e.g. `prop={self.foo}`
    ComponentParameter,
    /// General Rust code chunk
    General,
}

/// A bidirectional mapping between a range in the `.rs.html` template
/// and the corresponding range in the virtual `.rs` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingSpan {
    /// Source range in the `.rs.html` template document.
    pub template_range: Range,
    /// Target range in the virtual `.rs` generated document.
    pub virtual_range: Range,
    /// Semantic kind of this span.
    pub kind: SpanKind,
}

impl MappingSpan {
    pub fn new(template_range: Range, virtual_range: Range, kind: SpanKind) -> Self {
        Self {
            template_range,
            virtual_range,
            kind,
        }
    }

    /// Checks if a position is within the template range (inclusive start, inclusive/exclusive end).
    pub fn contains_template_pos(&self, pos: Position) -> bool {
        Self::position_in_range(pos, self.template_range)
    }

    /// Checks if a position is within the virtual range.
    pub fn contains_virtual_pos(&self, pos: Position) -> bool {
        Self::position_in_range(pos, self.virtual_range)
    }

    /// Translates a position inside this span from template coordinates to virtual coordinates.
    pub fn translate_to_virtual(&self, pos: Position) -> Option<Position> {
        if !self.contains_template_pos(pos) {
            return None;
        }

        let line_offset = pos.line - self.template_range.start.line;
        let char_offset = if line_offset == 0 {
            let base_offset = pos.character - self.template_range.start.character;
            // If it's an AtExpression and pos is after the '@', we adjust for the consumed '@'
            if self.kind == SpanKind::AtExpression {
                base_offset.saturating_sub(1)
            } else {
                base_offset
            }
        } else {
            pos.character
        };

        Some(Position {
            line: self.virtual_range.start.line + line_offset,
            character: if line_offset == 0 {
                self.virtual_range.start.character + char_offset
            } else {
                char_offset
            },
        })
    }

    /// Translates a position inside this span from virtual coordinates back to template coordinates.
    pub fn translate_to_template(&self, pos: Position) -> Option<Position> {
        if !self.contains_virtual_pos(pos) {
            return None;
        }

        let line_offset = pos.line - self.virtual_range.start.line;
        let char_offset = if line_offset == 0 {
            let base_offset = pos.character - self.virtual_range.start.character;
            if self.kind == SpanKind::AtExpression {
                base_offset + 1
            } else {
                base_offset
            }
        } else {
            pos.character
        };

        Some(Position {
            line: self.template_range.start.line + line_offset,
            character: if line_offset == 0 {
                self.template_range.start.character + char_offset
            } else {
                char_offset
            },
        })
    }

    fn position_in_range(pos: Position, range: Range) -> bool {
        if pos.line < range.start.line || pos.line > range.end.line {
            return false;
        }
        if pos.line == range.start.line && pos.character < range.start.character {
            return false;
        }
        if pos.line == range.end.line && pos.character > range.end.character {
            return false;
        }
        true
    }
}

/// Manages bidirectional coordinate mapping between `.rs.html` template files
/// and the transpiled virtual Rust file wrapped in `__rshtml_virtual_context(&self)`.
#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    /// Number of lines inserted in the virtual header before template body starts (fallback).
    pub header_lines_count: usize,
    /// Indentation spaces prepended to each line of the template body inside the function (fallback).
    pub indent_spaces: usize,
    /// Fine-grained mapping spans for precise span-level translation.
    pub spans: Vec<MappingSpan>,
}

impl SourceMap {
    /// Creates a new source map with known header line count and indentation.
    pub fn new(header_lines_count: usize, indent_spaces: usize) -> Self {
        Self {
            header_lines_count,
            indent_spaces,
            spans: Vec::new(),
        }
    }

    /// Creates a new source map with detailed mapping spans.
    pub fn with_spans(
        header_lines_count: usize,
        indent_spaces: usize,
        spans: Vec<MappingSpan>,
    ) -> Self {
        Self {
            header_lines_count,
            indent_spaces,
            spans,
        }
    }

    /// Adds a mapping span to the source map.
    pub fn add_span(&mut self, span: MappingSpan) {
        self.spans.push(span);
    }

    /// Scans a Tree-sitter AST and constructs high-precision `MappingSpan`s
    /// for all Rust expressions, blocks, statements and component properties.
    pub fn build_from_tree(
        tree: &Tree,
        _source: &str,
        header_lines_count: usize,
        indent_spaces: usize,
    ) -> Self {
        let mut spans = Vec::new();
        let root = tree.root_node();
        let mut cursor = root.walk();

        Self::collect_spans_recursive(&mut cursor, header_lines_count, indent_spaces, &mut spans);

        Self {
            header_lines_count,
            indent_spaces,
            spans,
        }
    }

    fn collect_spans_recursive(
        cursor: &mut tree_sitter::TreeCursor,
        header_lines_count: usize,
        indent_spaces: usize,
        spans: &mut Vec<MappingSpan>,
    ) {
        loop {
            let node = cursor.node();
            let kind = node.kind();

            match kind {
                "rust_expr_simple" => {
                    let mut is_at_expr = false;
                    let mut template_start_pos = Position::new(
                        node.start_position().row as u32,
                        node.start_position().column as u32,
                    );

                    if let Some(parent) = node.parent() {
                        let mut p_cursor = parent.walk();
                        for child in parent.children(&mut p_cursor) {
                            if child.kind() == "start_symbol"
                                && child.end_position() == node.start_position()
                            {
                                is_at_expr = true;
                                template_start_pos = Position::new(
                                    child.start_position().row as u32,
                                    child.start_position().column as u32,
                                );
                                break;
                            }
                        }
                    }

                    let template_end_pos = Position::new(
                        node.end_position().row as u32,
                        node.end_position().column as u32,
                    );

                    let virtual_start_pos = Position::new(
                        node.start_position().row as u32 + header_lines_count as u32,
                        node.start_position().column as u32 + indent_spaces as u32,
                    );
                    let virtual_end_pos = Position::new(
                        node.end_position().row as u32 + header_lines_count as u32,
                        node.end_position().column as u32 + indent_spaces as u32,
                    );

                    spans.push(MappingSpan::new(
                        Range::new(template_start_pos, template_end_pos),
                        Range::new(virtual_start_pos, virtual_end_pos),
                        if is_at_expr {
                            SpanKind::AtExpression
                        } else {
                            SpanKind::General
                        },
                    ));
                }
                "rust_block" => {
                    let template_range = Range::new(
                        Position::new(
                            node.start_position().row as u32,
                            node.start_position().column as u32,
                        ),
                        Position::new(
                            node.end_position().row as u32,
                            node.end_position().column as u32,
                        ),
                    );
                    let virtual_range = Range::new(
                        Position::new(
                            node.start_position().row as u32 + header_lines_count as u32,
                            node.start_position().column as u32 + indent_spaces as u32,
                        ),
                        Position::new(
                            node.end_position().row as u32 + header_lines_count as u32,
                            node.end_position().column as u32 + indent_spaces as u32,
                        ),
                    );
                    spans.push(MappingSpan::new(
                        template_range,
                        virtual_range,
                        SpanKind::RustBlock,
                    ));
                }
                "component_tag_parameter" => {
                    let mut cursor_child = node.walk();
                    for child in node.children(&mut cursor_child) {
                        if child.kind() == "_inner_template" || child.kind().contains("rust") {
                            let template_range = Range::new(
                                Position::new(
                                    child.start_position().row as u32,
                                    child.start_position().column as u32,
                                ),
                                Position::new(
                                    child.end_position().row as u32,
                                    child.end_position().column as u32,
                                ),
                            );
                            let virtual_range = Range::new(
                                Position::new(
                                    child.start_position().row as u32 + header_lines_count as u32,
                                    child.start_position().column as u32 + indent_spaces as u32,
                                ),
                                Position::new(
                                    child.end_position().row as u32 + header_lines_count as u32,
                                    child.end_position().column as u32 + indent_spaces as u32,
                                ),
                            );
                            spans.push(MappingSpan::new(
                                template_range,
                                virtual_range,
                                SpanKind::ComponentParameter,
                            ));
                        }
                    }
                }
                _ => {}
            }

            if cursor.goto_first_child() {
                Self::collect_spans_recursive(cursor, header_lines_count, indent_spaces, spans);
                cursor.goto_parent();
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    /// Converts a cursor position from `.rs.html` template coordinates to virtual Rust coordinates.
    /// span translation, falling back to uniform offset translation.
    pub fn template_to_virtual(&self, template_pos: Position) -> Position {
        for span in &self.spans {
            if let Some(mapped) = span.translate_to_virtual(template_pos) {
                return mapped;
            }
        }

        // Linear fallback
        Position {
            line: template_pos.line + self.header_lines_count as u32,
            character: template_pos.character + self.indent_spaces as u32,
        }
    }

    /// Converts a position from virtual Rust coordinates back to `.rs.html` template coordinates.
    /// span translation, falling back to uniform offset translation.
    pub fn virtual_to_template(&self, virtual_pos: Position) -> Option<Position> {
        for span in &self.spans {
            if let Some(mapped) = span.translate_to_template(virtual_pos) {
                return Some(mapped);
            }
        }

        // Linear fallback
        if (virtual_pos.line as usize) < self.header_lines_count {
            return None;
        }

        let mapped_line = virtual_pos.line - self.header_lines_count as u32;
        let mapped_char = virtual_pos
            .character
            .saturating_sub(self.indent_spaces as u32);

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
    use tree_sitter::Parser;

    fn parse_rshtml(source: &str) -> Tree {
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_rshtml::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        parser.parse(source, None).unwrap()
    }

    #[test]
    fn test_template_to_virtual_mapping_fallback() {
        let sm = SourceMap::new(10, 8);

        let template_pos = Position::new(5, 12);
        let virt_pos = sm.template_to_virtual(template_pos);

        assert_eq!(virt_pos.line, 15);
        assert_eq!(virt_pos.character, 20);
    }

    #[test]
    fn test_virtual_to_template_mapping_fallback() {
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
    fn test_range_conversion_fallback() {
        let sm = SourceMap::new(10, 8);

        let virt_range = Range {
            start: Position::new(12, 10),
            end: Position::new(12, 25),
        };

        let template_range = sm
            .virtual_to_template_range(virt_range)
            .expect("Must map range");
        assert_eq!(template_range.start, Position::new(2, 2));
        assert_eq!(template_range.end, Position::new(2, 17));
    }

    #[test]
    fn test_at_expression_mapping_span() {
        // Template line 4: `    @self.title`
        // Range in template: (4, 4) to (4, 15)  [including '@']
        // Virtual code line 18: `        self.title`
        // Range in virtual:  (18, 8) to (18, 18) [without '@']
        let span = MappingSpan::new(
            Range::new(Position::new(4, 4), Position::new(4, 15)),
            Range::new(Position::new(18, 8), Position::new(18, 18)),
            SpanKind::AtExpression,
        );

        let sm = SourceMap::with_spans(14, 4, vec![span]);

        // 1. Cursor in template right after `@sel` (line 4, character 8)
        let template_cursor = Position::new(4, 8);
        let virt_cursor = sm.template_to_virtual(template_cursor);
        assert_eq!(virt_cursor.line, 18);
        assert_eq!(virt_cursor.character, 11);

        // 2. Downstream RA returns a replacement range for `sel`: (18, 8) to (18, 11)
        let virt_replacement = Range::new(Position::new(18, 8), Position::new(18, 11));
        let mapped_template_range = sm
            .virtual_to_template_range(virt_replacement)
            .expect("Must map replacement range back to template");

        assert_eq!(mapped_template_range.start, Position::new(4, 5));
        assert_eq!(mapped_template_range.end, Position::new(4, 8));
    }

    #[test]
    fn test_component_parameter_mapping_span() {
        let span = MappingSpan::new(
            Range::new(Position::new(2, 15), Position::new(2, 25)),
            Range::new(Position::new(12, 8), Position::new(12, 18)),
            SpanKind::ComponentParameter,
        );

        let sm = SourceMap::with_spans(10, 0, vec![span]);

        let template_pos = Position::new(2, 19);
        let virt_pos = sm.template_to_virtual(template_pos);
        assert_eq!(virt_pos.line, 12);
        assert_eq!(virt_pos.character, 12);

        let virt_range = Range::new(Position::new(12, 8), Position::new(12, 12));
        let template_range = sm.virtual_to_template_range(virt_range).unwrap();
        assert_eq!(template_range.start, Position::new(2, 15));
        assert_eq!(template_range.end, Position::new(2, 19));
    }

    #[test]
    fn test_build_from_tree_automatically_extracts_at_expressions_and_blocks() {
        let source = r#"<div>
    <p>@self.title</p>
    @{
        let val = 42;
    }
    <Button label={self.btn_text} />
</div>"#;

        let tree = parse_rshtml(source);
        let sm = SourceMap::build_from_tree(&tree, source, 15, 0);

        assert!(
            !sm.spans.is_empty(),
            "Expected spans to be extracted from tree"
        );

        let title_span = sm
            .spans
            .iter()
            .find(|s| s.kind == SpanKind::AtExpression)
            .expect("Must find @self.title AtExpression span");

        assert_eq!(title_span.template_range.start.line, 1);
        assert_eq!(title_span.template_range.start.character, 7); // `@` position
        assert_eq!(title_span.template_range.end.character, 18); // end of `title`

        // Check translation of `@sel` (character 11: 7 is '@', 8 is 's', 9 is 'e', 10 is 'l', cursor at 11)
        // virt_start is col 8 (node start). base_offset is 11 - 7 = 4. Minus 1 for '@' = offset 3.
        // virt_cursor is col 8 + 3 = 11!
        let template_cursor = Position::new(1, 11);
        let virt_cursor = sm.template_to_virtual(template_cursor);
        assert_eq!(virt_cursor.line, 16);
        assert_eq!(virt_cursor.character, 11);

        // When RA returns replacement range for `sel`: (16, 8) to (16, 11)
        let virt_rep = Range::new(Position::new(16, 8), Position::new(16, 11));
        let mapped = sm.virtual_to_template_range(virt_rep).unwrap();
        assert_eq!(mapped.start, Position::new(1, 8));
        assert_eq!(mapped.end, Position::new(1, 11));
    }
}
