use rshtml_signature::ResolvedViewContext;
use std::fmt::Write;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct TranspiledOutput {
    /// Complete virtual Rust code including imports and context function.
    pub virtual_code: String,
    /// Extracted template Rust body.
    pub template_body: String,
    /// Total lines prepended before the template body (for 1:1 line offset calculations).
    pub header_lines_count: usize,
}

pub struct TemplateTranspiler;

impl TemplateTranspiler {
    /// Transpiles a `.rs.html` template using the official `rshtml_core` parser
    /// and wraps it in a valid associated `impl <StructName>` block with inherited `use` statements.
    pub fn transpile_file(
        template_file: &Path,
        base_dir: &Path,
        context: &ResolvedViewContext,
    ) -> Result<TranspiledOutput, String> {
        Self::transpile_file_with_tree(template_file, base_dir, context, None, None)
    }

    pub fn transpile_file_with_tree(
        template_file: &Path,
        base_dir: &Path,
        context: &ResolvedViewContext,
        tree_opt: Option<&tree_sitter::Tree>,
        source_opt: Option<&str>,
    ) -> Result<TranspiledOutput, String> {
        let field_names: Vec<String> = context.fields.iter().map(|f| f.name.clone()).collect();

        let (_fn_signs, _fn_bodies, _include_strs, info, _fn_name) =
            rshtml_core::rshtml_file::compile(template_file, base_dir, &field_names, true)?;

        let mut debug = info.debug;
        debug.extract();

        let extracted_bytes = debug.source;
        let mut template_body = String::from_utf8(extracted_bytes).unwrap_or_default();

        if let (Some(tree), Some(source)) = (tree_opt, source_opt) {
            template_body = Self::enrich_incomplete_expressions(tree, source, &template_body);
            template_body = Self::enrich_component_expressions(tree, source, &template_body);
        }

        let output = Self::wrap_with_context(&template_body, context);
        Ok(output)
    }

    
    pub fn enrich_incomplete_expressions(
        tree: &tree_sitter::Tree,
        source: &str,
        template_body: &str,
    ) -> String {
        let mut lines: Vec<String> = template_body.lines().map(|s| s.to_string()).collect();
        let root = tree.root_node();
        let mut cursor = root.walk();

        Self::inject_incomplete_dots_recursive(&mut cursor, source, &mut lines);

        lines.join("\n")
    }

    fn inject_incomplete_dots_recursive(
        cursor: &mut tree_sitter::TreeCursor,
        source: &str,
        lines: &mut [String],
    ) {
        loop {
            let node = cursor.node();
            // Look for ERROR nodes containing dot right after a rust expression
            if node.kind() == "ERROR" {
                if let Ok(err_text) = node.utf8_text(source.as_bytes()) {
                    if err_text.trim_start().starts_with('.') {
                        if let Some(prev) = node.prev_sibling() {
                            if prev.kind() == "rust_expr_simple" || prev.kind() == "rust_text" {
                                let dot_pos = node.start_position();
                                if dot_pos.row < lines.len() {
                                    let line = &mut lines[dot_pos.row];
                                    let col = dot_pos.column;
                                    // If there is a semicolon right at or after col (e.g. &self;), replace it with dot
                                    if col < line.len() {
                                        let bytes = line.as_bytes();
                                        if bytes[col] == b';' {
                                            line.replace_range(col..col + 1, ".");
                                        } else if col > 0 && bytes[col - 1] == b';' {
                                            line.replace_range(col - 1..col, ".");
                                        } else if let Some(semi_idx) = line[col..].find(';') {
                                            let target = col + semi_idx;
                                            line.replace_range(target..target + 1, ".");
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if cursor.goto_first_child() {
                Self::inject_incomplete_dots_recursive(cursor, source, lines);
                cursor.goto_parent();
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    pub fn enrich_component_expressions(
        tree: &tree_sitter::Tree,
        source: &str,
        template_body: &str,
    ) -> String {
        let mut lines: Vec<String> = template_body.lines().map(|s| s.to_string()).collect();
        let root = tree.root_node();
        let mut cursor = root.walk();

        Self::inject_comp_params_recursive(&mut cursor, source, &mut lines);

        lines.join("
")
    }

    fn inject_comp_params_recursive(
        cursor: &mut tree_sitter::TreeCursor,
        source: &str,
        lines: &mut [String],
    ) {
        loop {
            let node = cursor.node();
            if node.kind() == "component_tag_parameter" {
                let has_brace = {
                    let mut ck = node.walk();
                    node.children(&mut ck).any(|c| c.kind() == "open_brace")
                };
                if has_brace {
                    if let Some(body_node) = node.child_by_field_name("body") {
                        let start = body_node.start_position();
                        let end = body_node.end_position();
                        if start.row == end.row && start.row < lines.len() {
                            let row = start.row;
                            let col_start = start.column;
                            let col_end = end.column;

                            if let Ok(raw_text) = body_node.utf8_text(source.as_bytes()) {
                                let inner = raw_text.trim();
                                if !inner.is_empty() {
                                    let line = &mut lines[row];
                                    if line.len() < col_end {
                                        let needed = col_end - line.len();
                                        line.extend(std::iter::repeat(' ').take(needed));
                                    }

                                    let slice = &line[col_start..col_end];
                                    if slice.trim().is_empty() {
                                        let injected = format!("&{};", inner);
                                        let width = col_end - col_start;
                                        let padded = if injected.len() <= width {
                                            format!("{:<width$}", injected, width = width)
                                        } else {
                                            injected
                                        };

                                        line.replace_range(col_start..col_end, &padded);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if cursor.goto_first_child() {
                Self::inject_comp_params_recursive(cursor, source, lines);
                cursor.goto_parent();
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    /// Wraps the template context with an empty body (testing if rust-analyzer keeps struct context)
    pub fn wrap_empty_context(context: &ResolvedViewContext) -> TranspiledOutput {
        Self::wrap_with_context("", context)
    }

    /// Wraps the extracted template Rust body in a complete virtual file context.
    pub fn wrap_with_context(
        template_body: &str,
        context: &ResolvedViewContext,
    ) -> TranspiledOutput {
        let mut virtual_code = String::new();

        // 1. Module configuration
        writeln!(
            virtual_code,
            "// @generated by rshtml-analyzer from rshtml_core"
        )
        .unwrap();
        writeln!(
            virtual_code,
            "#![allow(unused_imports, dead_code, unused_variables, path_statements)]"
        )
        .unwrap();
        writeln!(virtual_code).unwrap();

        // 2. Inherited `use` statements
        for u in &context.use_statements {
            writeln!(virtual_code, "{}", u).unwrap();
        }
        writeln!(virtual_code).unwrap();

        // 3. Import target struct if not already in file use statements
        if !context
            .use_statements
            .iter()
            .any(|u| u.contains(&context.struct_name))
        {
            writeln!(virtual_code, "use crate::{};", context.struct_name).unwrap();
            writeln!(virtual_code).unwrap();
        }

        // 4. Begin impl block with full generics and where clause support
        let impl_gen_str = match &context.impl_generics {
            Some(g) if !g.trim().is_empty() => format!("{}", g.trim()),
            _ => String::new(),
        };
        let ty_gen_str = match &context.ty_generics {
            Some(g) if !g.trim().is_empty() => g.trim().to_string(),
            _ => String::new(),
        };
        let where_cl_str = match &context.where_clause {
            Some(w) if !w.trim().is_empty() => format!(" {}", w.trim()),
            _ => String::new(),
        };

        if impl_gen_str.is_empty() {
            writeln!(
                virtual_code,
                "impl {}{}{} {{",
                context.struct_name, ty_gen_str, where_cl_str
            )
            .unwrap();
        } else {
            writeln!(
                virtual_code,
                "impl{} {}{}{} {{",
                impl_gen_str, context.struct_name, ty_gen_str, where_cl_str
            )
            .unwrap();
        }

        writeln!(
            virtual_code,
            "    pub fn __rshtml_virtual_context(&self) {{"
        )
        .unwrap();

        let header_lines_count = virtual_code.lines().count();

        // 5. Injected template body (preserves exact line spacing from rshtml_core!)
        if !template_body.is_empty() {
            writeln!(virtual_code, "{}", template_body).unwrap();
        }

        // 6. Close function and impl
        writeln!(virtual_code, "    }}").unwrap();
        writeln!(virtual_code, "}}").unwrap();

        TranspiledOutput {
            virtual_code,
            template_body: template_body.to_string(),
            header_lines_count,
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_inspect_incomplete_expression_enrichment() {
        let source = "<div>
    @self.
</div>";
        let mut parser = tree_sitter::Parser::new();
        let lang: tree_sitter::Language = tree_sitter_rshtml::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(source, None).unwrap();

        let root = tree.root_node();
        println!("S-EXPR: {}", root.to_sexp());

        let template_body = "
    &self;
";
        let enriched = TemplateTranspiler::enrich_incomplete_expressions(&tree, source, template_body);
        println!("ENRICHED BODY:
{:?}", enriched);
    }


    #[test]
    fn test_inspect_incomplete_self_dot_extraction() {
        let temp_dir = std::env::temp_dir().join(format!("inspect_dot_{}", std::process::id()));
        let views_dir = temp_dir.join("views");
        let _ = std::fs::create_dir_all(&views_dir);

        let template_content = "<div>
    @self.
</div>";
        let template_path = views_dir.join("page.rs.html");
        let _ = std::fs::write(&template_path, template_content);

        let context = ResolvedViewContext {
            struct_name: "IndexPage".to_string(),
            impl_generics: None,
            ty_generics: None,
            where_clause: None,
            rust_file_path: std::path::PathBuf::from("src/main.rs"),
            use_statements: vec![],
            fields: vec![],
        };

        let result = TemplateTranspiler::transpile_file(&template_path, &views_dir, &context);
        if let Ok(out) = result {
            println!("EXTRACTED VIRTUAL CODE:
{}", out.virtual_code);
            for (i, line) in out.virtual_code.lines().enumerate() {
                println!("L{:02}: {}", i, line);
            }
        }
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    use super::*;
    use rshtml_signature::StructFieldInfo;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn test_wrap_empty_context() {
        let context = ResolvedViewContext {
            struct_name: "IndexPage".to_string(),
            impl_generics: None,
            ty_generics: None,
            where_clause: None,
            rust_file_path: PathBuf::from("src/main.rs"),
            use_statements: vec![],
            fields: vec![],
        };

        let output = TemplateTranspiler::wrap_empty_context(&context);
        assert!(output.virtual_code.contains("impl IndexPage {"));
        assert!(
            output
                .virtual_code
                .contains("pub fn __rshtml_virtual_context(&self) {")
        );
        assert_eq!(output.template_body, "");
    }

    #[test]
    fn test_formatting_independence() {
        let temp_dir = std::env::temp_dir().join(format!("fmt_test_{}", std::process::id()));
        let views_dir = temp_dir.join("views");
        fs::create_dir_all(&views_dir).unwrap();

        let template_content = "<div>\n    \n    @if    self.footer    {\n        <p>\n            @self.home_time.year()\n        </p>\n    }\n</div>";
        let template_path = views_dir.join("test_fmt.rs.html");
        fs::write(&template_path, template_content).unwrap();

        let context = ResolvedViewContext {
            struct_name: "IndexPage".to_string(),
            impl_generics: None,
            ty_generics: None,
            where_clause: None,
            rust_file_path: PathBuf::from("src/main.rs"),
            use_statements: vec![],
            fields: vec![
                StructFieldInfo {
                    name: "footer".to_string(),
                    field_type: "bool".to_string(),
                },
                StructFieldInfo {
                    name: "home_time".to_string(),
                    field_type: "DateTime<Utc>".to_string(),
                },
            ],
        };

        let result =
            TemplateTranspiler::transpile_file(&template_path, &views_dir, &context).unwrap();
        let orig_lines: Vec<&str> = template_content.lines().collect();
        let extracted_lines: Vec<&str> = result.template_body.lines().collect();

        assert_eq!(
            orig_lines.len(),
            extracted_lines.len(),
            "O número de linhas deve ser estritamente idêntico"
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_generics_wrap_with_context() {
        let context = ResolvedViewContext {
            struct_name: "TableView".to_string(),
            impl_generics: Some("<'a, T: std::fmt::Display, U>".to_string()),
            ty_generics: Some("<'a, T, U>".to_string()),
            where_clause: Some("where U: Clone".to_string()),
            rust_file_path: PathBuf::from("src/main.rs"),
            use_statements: vec!["use std::fmt::Display;".to_string()],
            fields: vec![],
        };

        let output = TemplateTranspiler::wrap_with_context("    let _ = self;", &context);
        assert!(
            output
                .virtual_code
                .contains("impl<'a, T: std::fmt::Display, U> TableView<'a, T, U> where U: Clone {"),
            "Expected exact generic impl block, got: \n{}",
            output.virtual_code
        );
    }

    #[test]
    fn test_transpile_via_rshtml_core() {
        let temp_dir =
            std::env::temp_dir().join(format!("rshtml_core_transpile_test_{}", std::process::id()));
        let views_dir = temp_dir.join("views");
        fs::create_dir_all(&views_dir).unwrap();

        let template_content = r#"<div>
    @if self.footer {
        <p>Copyright @self.home_time.year() RsHtml</p>
    }
</div>"#;
        let template_path = views_dir.join("index.rs.html");
        fs::write(&template_path, template_content).unwrap();

        let context = ResolvedViewContext {
            struct_name: "IndexPage".to_string(),
            impl_generics: None,
            ty_generics: None,
            where_clause: None,
            rust_file_path: PathBuf::from("src/main.rs"),
            use_statements: vec![
                "use chrono::{DateTime, Datelike, Utc};".to_string(),
                "use rshtml::View;".to_string(),
            ],
            fields: vec![
                StructFieldInfo {
                    name: "footer".to_string(),
                    field_type: "bool".to_string(),
                },
                StructFieldInfo {
                    name: "home_time".to_string(),
                    field_type: "DateTime<Utc>".to_string(),
                },
            ],
        };

        let result = TemplateTranspiler::transpile_file(&template_path, &views_dir, &context);
        assert!(
            result.is_ok(),
            "transpile_file should succeed: {:?}",
            result.err()
        );

        let output = result.unwrap();
        assert!(output.virtual_code.contains("impl IndexPage {"));
        assert!(output.virtual_code.contains("if self.footer {"));
        assert!(output.virtual_code.contains("&self.home_time.year();"));

        let _ = fs::remove_dir_all(temp_dir);
    }
}
