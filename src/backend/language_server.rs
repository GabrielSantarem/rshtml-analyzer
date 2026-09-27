use crate::app_state::view::View;
use crate::backend::Backend;
use crate::backend::navigation_target::NavigationTarget;
use crate::backend::server_capabilities::{semantic_tokens_capabilities, workspace_capabilities};
use crate::backend::syntax_context::SyntaxContext;
use crate::backend::tree_extensions::TreeExtensions;
use crate::consts;
use tower_lsp::jsonrpc::Error;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionList, CompletionOptions, CompletionParams,
    CompletionResponse, CompletionTextEdit, TextEdit, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, HoverProviderCapability,
    InitializeParams, InitializeResult, InitializedParams, InsertTextFormat, Location,
    MarkupContent, MarkupKind, MessageType, OneOf, Range, SemanticTokens, SemanticTokensDelta,
    SemanticTokensDeltaParams, SemanticTokensFullDeltaResult, SemanticTokensParams,
    SemanticTokensRangeParams, SemanticTokensRangeResult, SemanticTokensResult, ServerCapabilities,
    ServerInfo, TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use tower_lsp::{LanguageServer, jsonrpc};
use crate::signature::SignatureResolver;
use crate::ra_proxy::RustAnalyzerProcess;
use tracing::{debug, error};
use crate::{log_info, log_error, log_debug};

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult, Error> {
        debug!(
            "The Initialize request has been received (build: {})...",
            consts::BUILD_MODE
        );

        let workspace_root_path = params
            .workspace_folders
            .as_ref()
            .and_then(|folders| folders.first())
            .and_then(|folder| folder.uri.to_file_path().ok())
            .or_else(|| params.root_uri.as_ref().and_then(|uri| uri.to_file_path().ok()));

        if let Some(path) = workspace_root_path {
            log_info!("WORKSPACE", "Workspace root path: {:?}", path);
            self.client
                .log_message(MessageType::INFO, format!("[rshtml-analyzer] Initializing workspace at {path:?}..."))
                .await;

            let mut workspace = self.state.workspace.write().await;
            if let Err(e) = workspace.load(&path) {
                log_error!("WORKSPACE", "Workspace failed to load: {}", e);
                self.client
                    .log_message(MessageType::ERROR, format!("[rshtml-analyzer] Workspace load error: {}", e))
                    .await;
            } else {
                log_info!("WORKSPACE", "Workspace loaded successfully with {} crate members", workspace.members.len());
            }

            // Iniciar downstream rust-analyzer com repasse de feedback de progresso para o editor
            self.client
                .log_message(MessageType::INFO, "[rshtml-analyzer] Starting downstream rust-analyzer...".to_string())
                .await;

            match RustAnalyzerProcess::spawn(path.to_str()).await {
                Ok(ra) => {
                    log_info!("RA_INIT", "rust-analyzer spawned and initialized downstream successfully");
                    self.client
                        .log_message(MessageType::INFO, "[rshtml-analyzer] rust-analyzer connected downstream!".to_string())
                        .await;
                    let mut ra_lock = self.state.ra_process.write().await;
                    *ra_lock = Some(ra);
                }
                Err(e) => {
                    log_error!("RA_INIT", "Failed to start downstream rust-analyzer: {}", e);
                    self.client
                        .log_message(MessageType::ERROR, format!("[rshtml-analyzer] Failed to start rust-analyzer downstream: {}", e))
                        .await;
                }
            }
        }

        debug!("Sending an initialize response.");
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                semantic_tokens_provider: semantic_tokens_capabilities(),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    trigger_characters: Some(vec!["@".to_string(), "<".to_string(), ".".to_string()]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                workspace: workspace_capabilities(),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "rshtml-analyzer".to_string(),
                version: Some(format!(
                    "{} ({})",
                    env!("CARGO_PKG_VERSION"),
                    consts::BUILD_MODE
                )),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let version_msg = format!(
            "rshtml-analyzer v{} ({}) connected!",
            env!("CARGO_PKG_VERSION"),
            consts::BUILD_MODE
        );

        if cfg!(debug_assertions) {
            self.client
                .show_message(MessageType::INFO, &version_msg)
                .await;
        }

        self.client
            .log_message(
                MessageType::INFO,
                format!("{version_msg} Handshake completed."),
            )
            .await;
    }

    async fn shutdown(&self) -> Result<(), Error> {
        let mut ra_lock = self.state.ra_process.write().await;
        if let Some(ra) = ra_lock.take() {
            let _ = ra.shutdown().await;
        }
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let msg = format!("Opened file: {}", &params.text_document.uri);
        self.client.log_message(MessageType::INFO, msg).await;

        let uri_str = params.text_document.uri.to_string();
        let text = params.text_document.text;

        let tree = {
            let mut parser = self.state.parser.lock().await;

            if let Some(tree) = parser.parse(&text, None) {
                tree
            } else {
                self.client
                    .log_message(MessageType::ERROR, "Parser error: Couldn't create tree.")
                    .await;
                return;
            }
        };

        let use_directives = tree.find_uses(&self.state.language, &text);
        debug!("Use directives: {:?}", use_directives);

        let views_path_opt = self.get_views_path_for_uri(&params.text_document.uri).await;

        let mut use_directives_with_params = Vec::new();
        if let Some(ref views_path) = views_path_opt {
            for (use_path, use_name) in &use_directives {
                let use_params = self
                    .state
                    .find_use_params(&views_path.join(use_path))
                    .await
                    .unwrap_or(Vec::new());
                debug!("use params: {use_params:?}");
                use_directives_with_params.push((use_path.to_owned(), use_name.to_owned(), use_params))
            }
        }

        let template_params = tree.find_template_params(&self.state.language, &text);
        debug!("Template params: {:?}", template_params);

        let errors = {
            let mut view = View::new(text.clone(), tree, params.text_document.version as usize);
            view.use_directives = use_directives_with_params;
            view.create_use_directive_completion_items();
            view.template_params = template_params;

            let mut views = self.state.views.write().await;

            let errors = view.tree.find_error(&self.state.language, &view.source);

            views.insert(uri_str, view);

            errors
        };

        self.client
            .publish_diagnostics(
                params.text_document.uri.clone(),
                errors,
                Some(params.text_document.version),
            )
            .await;

        // Synchronize virtual file with downstream rust-analyzer
        if let Ok(file_path) = params.text_document.uri.to_file_path() {
            let workspace = self.state.workspace.read().await;
            if let Some(member) = workspace.get_member_by_view(&file_path) {
                let ra_lock = self.state.ra_process.read().await;
                if let Err(e) = self.state.virtual_files.sync_template(
                    &params.text_document.uri,
                    &text,
                    &member.path,
                    ra_lock.as_ref(),
                ).await {
                    log_error!("VFS", "did_open sync failed: {}", e);
                } else {
                    log_info!("VFS", "did_open synchronized virtual file for {}", &params.text_document.uri);
                }
            }
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let msg = format!("Changed file: {}", &params.text_document.uri);
        self.client.log_message(MessageType::INFO, msg).await;

        let uri_str = params.text_document.uri.to_string();

        let (new_uses, errors) = {
            let mut views = self.state.views.write().await;

            if let Some(view) = views.get_mut(&uri_str) {
                if view.version >= params.text_document.version as usize {
                    return;
                }

                self.process_changes(params.content_changes, &mut view.source, &mut view.tree);

                let tree = {
                    let mut parser = self.state.parser.lock().await;
                    if let Some(tree) = parser.parse(&view.source, Some(&view.tree)) {
                        tree
                    } else {
                        error!("Error while parsing tree");
                        return;
                    }
                };

                let use_directives = tree.find_uses(&self.state.language, &view.source);
                let template_params = tree.find_template_params(&self.state.language, &view.source);

                view.version = params.text_document.version as usize;
                view.tree = tree;
                let new_uses = view.sync_use_directives(use_directives);

                view.template_params = template_params;

                (
                    new_uses,
                    view.tree.find_error(&self.state.language, &view.source),
                )
            } else {
                error!("view {uri_str} not found");
                return;
            }
        };

        let views_path_opt = self.get_views_path_for_uri(&params.text_document.uri).await;

        let mut new_uses_params = Vec::new();
        if let Some(ref views_path) = views_path_opt {
            for (id, use_path) in new_uses {
                let use_params = self
                    .state
                    .find_use_params(&views_path.join(use_path))
                    .await
                    .unwrap_or_default();

                new_uses_params.push((id, use_params));
            }
        }

        {
            let mut views = self.state.views.write().await;
            if let Some(view) = views.get_mut(&uri_str) {
                for (id, use_params) in new_uses_params {
                    if let Some(use_directive) = view.use_directives.get_mut(id) {
                        use_directive.2 = use_params;
                    }
                }

                view.update_use_directive_completion_items();
            }
        }

        self.client
            .publish_diagnostics(
                params.text_document.uri.clone(),
                errors,
                Some(params.text_document.version),
            )
            .await;

        // Synchronize updated virtual file with downstream rust-analyzer
        if let Ok(file_path) = params.text_document.uri.to_file_path() {
            let views = self.state.views.read().await;
            if let Some(view) = views.get(&uri_str) {
                let updated_source = view.source.clone();
                drop(views);

                let workspace = self.state.workspace.read().await;
                if let Some(member) = workspace.get_member_by_view(&file_path) {
                    let ra_lock = self.state.ra_process.read().await;
                    if let Err(e) = self.state.virtual_files.sync_template(
                        &params.text_document.uri,
                        &updated_source,
                        &member.path,
                        ra_lock.as_ref(),
                    ).await {
                        log_error!("VFS", "did_change sync failed: {}", e);
                    }
                }
            }
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let msg = format!("Closed file: {}", &params.text_document.uri);
        self.client.log_message(MessageType::INFO, msg).await;
        let uri_str = params.text_document.uri.to_string();

        let mut views = self.state.views.write().await;
        views.remove(&uri_str);
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>, Error> {
        let uri_str = params.text_document.uri.to_string();

        let mut views = self.state.views.write().await;

        if let Some(view) = views.get_mut(&uri_str) {
            let highlight = &self.state.highlight;
            let tokens = highlight.highlight(&view.source, None)?;

            debug!("Semantic Tokens: {:?}", tokens.len());

            view.semantic_tokens_version += 1;

            let semantic_tokens = SemanticTokens {
                result_id: Some(view.semantic_tokens_version.to_string()),
                data: tokens,
            };
            view.semantic_tokens = semantic_tokens.clone();

            return Ok(Some(SemanticTokensResult::Tokens(semantic_tokens)));
        }

        Ok(None)
    }

    async fn semantic_tokens_full_delta(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> jsonrpc::Result<Option<SemanticTokensFullDeltaResult>> {
        let uri_str = params.text_document.uri.to_string();
        let result_id = params.previous_result_id;

        let mut views = self.state.views.write().await;

        if let Some(view) = views.get_mut(&uri_str) {
            let highlight = &self.state.highlight;
            let tokens = highlight.highlight(&view.source, None)?;

            if view.semantic_tokens.result_id.as_ref() != Some(&result_id) {
                debug!("Semantic Tokens Delta | Full: {:?}", tokens.len());
                view.semantic_tokens_version += 1;
                let semantic_tokens = SemanticTokens {
                    result_id: Some(view.semantic_tokens_version.to_string()),
                    data: tokens.clone(),
                };
                view.semantic_tokens = semantic_tokens.clone();
                return Ok(Some(SemanticTokensFullDeltaResult::Tokens(semantic_tokens)));
            }

            let tokens_diff =
                highlight.semantic_tokens_difference(&view.semantic_tokens.data, &tokens);

            debug!(
                "Semantic Tokens Delta: {:?} {:?}",
                tokens_diff.len(),
                tokens_diff
            );

            view.semantic_tokens_version += 1;
            view.semantic_tokens = SemanticTokens {
                result_id: Some(view.semantic_tokens_version.to_string()),
                data: tokens.clone(),
            };

            return Ok(Some(SemanticTokensFullDeltaResult::TokensDelta(
                SemanticTokensDelta {
                    result_id: Some(view.semantic_tokens_version.to_string()),
                    edits: tokens_diff,
                },
            )));
        }

        Ok(None)
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> jsonrpc::Result<Option<SemanticTokensRangeResult>> {
        let uri_str = params.text_document.uri.to_string();
        let range = params.range;

        let views = self.state.views.read().await;

        if let Some(view) = views.get(&uri_str) {
            let highlight = &self.state.highlight;
            let start_byte = Self::position_to_byte_offset(&view.source, range.start);
            let end_byte = Self::position_to_byte_offset(&view.source, range.end);
            let tokens = highlight.highlight(&view.source, Some(start_byte..end_byte))?;

            debug!("Semantic Tokens Range: {:?}", tokens.len());

            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: tokens,
            })));
        }
        Ok(None)
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> jsonrpc::Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let trigger_char = params
            .context
            .clone()
            .and_then(|ctx| ctx.trigger_character)
            .and_then(|s| s.chars().next());

        let views = self.state.views.read().await;

        if let Some(view) = views.get(&uri.to_string()) {
            let syntax_context = SyntaxContext::detect(&view.tree, &view.source, position);
            let mut completion_items: Vec<CompletionItem> = Vec::new();

            // Check if user is typing a field access on self (e.g. @self. or self.)
            let line_prefix = {
                let lines: Vec<&str> = view.source.lines().collect();
                if (position.line as usize) < lines.len() {
                    let line = lines[position.line as usize];
                    let col = (position.character as usize).min(line.len());
                    &line[..col]
                } else {
                    ""
                }
            };

            let is_self_dot = line_prefix.trim_end().ends_with("self.")
                || line_prefix.trim_end().ends_with("@self.")
                || trigger_char == Some('.');

            if is_self_dot || syntax_context == SyntaxContext::RustCode {
                // 1. Prioritize immediate struct fields
                if let Ok(file_path) = uri.to_file_path() {
                    let workspace = self.state.workspace.read().await;
                    if let Some(member) = workspace.get_member_by_view(&file_path) {
                        if let Ok(rel_path) = file_path.strip_prefix(&member.path) {
                            if let Some(resolved) = SignatureResolver::resolve(&member.path, rel_path.to_str().unwrap_or_default()) {
                                for field in resolved.fields {
                                    completion_items.push(CompletionItem {
                                        label: field.name.clone(),
                                        kind: Some(CompletionItemKind::FIELD),
                                        detail: Some(field.field_type),
                                        sort_text: Some(format!("0_{}", field.name)),
                                        ..Default::default()
                                    });
                                }
                            }
                        }
                    }
                }

                // 2. Query downstream rust-analyzer on the virtual file with translated coordinates
                if let Some(vdoc) = self.state.virtual_files.get_by_template_uri(&uri).await {
                    let virt_pos = vdoc.source_map.template_to_virtual(position);
                    let ra_lock = self.state.ra_process.read().await;
                    if let Some(ra) = ra_lock.as_ref() {
                        let ra_req = serde_json::json!({
                            "textDocument": { "uri": vdoc.virtual_uri.to_string() },
                            "position": { "line": virt_pos.line, "character": virt_pos.character },
                            "context": params.context
                        });

                        log_debug!("RA_COMPLETION", "Querying downstream RA at virtual line {} col {}", virt_pos.line, virt_pos.character);

                        if let Ok(ra_res) = ra.send_request("textDocument/completion", ra_req).await {
                            if let Some(items_val) = ra_res.pointer("/result/items").or_else(|| ra_res.pointer("/result")) {
                                if let Ok(mut ra_items) = serde_json::from_value::<Vec<CompletionItem>>(items_val.clone()) {
                                    log_debug!("RA_COMPLETION", "Received {} items from downstream RA", ra_items.len());
                                    // Remove internal virtual scaffolding method
                                    ra_items.retain(|item| !item.label.contains("__rshtml_virtual_"));

                                    for item in &mut ra_items {
                                        if let Some(ref sort) = item.sort_text {
                                            item.sort_text = Some(format!("1_{}", sort));
                                        }
                                        // Translate textEdit range from virtual Rust back to template coordinates
                                        if let Some(ref mut edit) = item.text_edit {
                                            match edit {
                                                CompletionTextEdit::Edit(TextEdit { range, .. }) => {
                                                    if let Some(mapped) = vdoc.source_map.virtual_to_template_range(*range) {
                                                        *range = mapped;
                                                    }
                                                }
                                                CompletionTextEdit::InsertAndReplace(ir) => {
                                                    if let Some(mapped_ins) = vdoc.source_map.virtual_to_template_range(ir.insert) {
                                                        ir.insert = mapped_ins;
                                                    }
                                                    if let Some(mapped_rep) = vdoc.source_map.virtual_to_template_range(ir.replace) {
                                                        ir.replace = mapped_rep;
                                                    }
                                                }
                                            }
                                        }
                                        // Translate additionalTextEdits ranges
                                        if let Some(ref mut add_edits) = item.additional_text_edits {
                                            for add_edit in add_edits {
                                                if let Some(mapped) = vdoc.source_map.virtual_to_template_range(add_edit.range) {
                                                    add_edit.range = mapped;
                                                }
                                            }
                                        }
                                    }
                                    completion_items.extend(ra_items);
                                }
                            }
                        }
                    }
                }
            }

            if syntax_context == SyntaxContext::Html {
                if let Some(tc) = trigger_char {
                    for (item_char, item) in view.completion_items.values() {
                        if *item_char == tc {
                            completion_items.push(item.clone());
                        }
                    }

                    if tc == '@' {
                        completion_items.extend(self.state.completion_items.clone());
                        completion_items.push(CompletionItem {
                            label: "self".to_string(),
                            kind: Some(CompletionItemKind::KEYWORD),
                            detail: Some("Template struct instance".to_string()),
                            sort_text: Some("0_self".to_string()),
                            ..Default::default()
                        });
                    }
                } else {
                    for (_, item) in view.completion_items.values() {
                        completion_items.push(item.clone());
                    }

                    completion_items.extend(self.state.completion_items.clone());
                }
            } else {
                match syntax_context {
                    SyntaxContext::ComponentTag => {
                        for (item_char, item) in view.completion_items.values() {
                            if *item_char == '<' {
                                completion_items.push(item.clone());
                            }
                        }
                    }
                    SyntaxContext::ComponentParameter(component_name) => {
                        for (name, params) in view.use_directives_names_and_params().into_iter() {
                            if name == component_name {
                                for param in params {
                                    completion_items.push(CompletionItem {
                                        insert_text: Some(format!("{}=\"$1\"", param)),
                                        detail: Some(format!("{} attribute", component_name)),
                                        sort_text: Some(format!("0_{}", param)),
                                        label: param,
                                        kind: Some(CompletionItemKind::PROPERTY),
                                        insert_text_format: Some(InsertTextFormat::SNIPPET),
                                        ..Default::default()
                                    });
                                }
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }

            log_info!("LSP_COMPLETION", "Returning {} items directly to editor!", completion_items.len());
            return Ok(Some(CompletionResponse::List(CompletionList {
                is_incomplete: true,
                items: completion_items,
            })));
        }

        debug!("Error while getting completion items");
        Ok(None)
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> jsonrpc::Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let views_path = match self.get_views_path_for_uri(uri).await {
            Some(path) => path,
            None => return Ok(None),
        };

        let views = self.state.views.read().await;
        if let Some(view) = views.get(&uri.to_string())
            && let Some(target) = NavigationTarget::resolve_at(
                &view.tree,
                &view.source,
                position,
                &view.use_directives,
                &views_path,
            )
        {
            let path = match target {
                NavigationTarget::Component { target_path, .. } => target_path,
                NavigationTarget::UseDirective { target_path, .. } => target_path,
            };

            if let Ok(uri) = Url::from_file_path(path) {
                return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                    uri,
                    range: Range::default(),
                })));
            }
        }

        Ok(None)
    }

    async fn hover(&self, params: HoverParams) -> jsonrpc::Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let views_path = match self.get_views_path_for_uri(uri).await {
            Some(path) => path,
            None => return Ok(None),
        };

        let views = self.state.views.read().await;
        if let Some(view) = views.get(&uri.to_string())
            && let Some(target) = NavigationTarget::resolve_at(
                &view.tree,
                &view.source,
                position,
                &view.use_directives,
                &views_path,
            )
        {
            let (range, markdown) = match target {
                NavigationTarget::Component {
                    name,
                    target_path,
                    range,
                    params,
                } => {
                    let params_doc = if params.is_empty() {
                        "_No parameters_".to_string()
                    } else {
                        params
                            .iter()
                            .map(|p| format!("- `{}`", p))
                            .collect::<Vec<_>>()
                            .join("\n")
                    };

                    let file_name = target_path
                        .file_name()
                        .and_then(|f| f.to_str())
                        .unwrap_or_default();

                    let doc = format!(
                        "### Component `<{name}>`\n\n**File:** `{file_name}`\n\n**Parameters:**\n{params_doc}"
                    );

                    (range, doc)
                }
                NavigationTarget::UseDirective {
                    path,
                    target_path: _,
                    range,
                } => {
                    let doc = format!("### Component Import\n\n`{path}`");
                    (range, doc)
                }
            };

            return Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: markdown,
                }),
                range: Some(range),
            }));
        }

        Ok(None)
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let cargo_toml_changed = params
            .changes
            .iter()
            .any(|event| event.uri.path().ends_with("/Cargo.toml"));

        if !cargo_toml_changed {
            return;
        }

        debug!("Cargo.toml changed. Re-analyzing...");

        let mut workspace = self.state.workspace.write().await;

        let root = workspace.root.clone();
        workspace.load(&root).unwrap_or_else(|e| {
            debug!("Workspace couldn't load: {}", e);
        });

        debug!("Workspace re-analysis complete.");
    }
}
