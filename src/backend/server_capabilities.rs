use crate::consts::{SEMANTIC_TOKEN_MODIFIERS, SEMANTIC_TOKEN_TYPES};
use tower_lsp::lsp_types::{
    CompletionOptions, HoverProviderCapability, OneOf, SemanticTokensFullOptions,
    SemanticTokensLegend, SemanticTokensOptions, SemanticTokensServerCapabilities,
    ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind,
    WorkDoneProgressOptions, WorkspaceFoldersServerCapabilities, WorkspaceServerCapabilities,
};

pub fn semantic_tokens_capabilities() -> Option<SemanticTokensServerCapabilities> {
    let legend = SemanticTokensLegend {
        token_types: SEMANTIC_TOKEN_TYPES.to_vec(),
        token_modifiers: SEMANTIC_TOKEN_MODIFIERS.to_vec(),
    };

    Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
        SemanticTokensOptions {
            work_done_progress_options: WorkDoneProgressOptions {
                work_done_progress: None,
            },
            legend,
            range: Some(true),
            full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
            // full: Some(SemanticTokensFullOptions::Bool(true)),
        },
    ))
}

pub fn workspace_capabilities() -> Option<WorkspaceServerCapabilities> {
    Some(WorkspaceServerCapabilities {
        workspace_folders: Some(WorkspaceFoldersServerCapabilities {
            supported: Some(false),
            change_notifications: Some(OneOf::Left(true)),
        }),

        file_operations: None,
    })
}

/// Builds the composite ServerCapabilities by preserving downstream rust-analyzer capabilities
/// (e.g. type definitions, references, formatting, code actions, rename, inlay hints)
/// while overlaying rshtml-specific handlers (Tree-sitter semantic tokens, template triggers `@`, `<`, `.`, etc.).
pub fn build_server_capabilities(downstream: Option<ServerCapabilities>) -> ServerCapabilities {
    let mut capabilities = downstream.unwrap_or_default();

    capabilities.text_document_sync = Some(TextDocumentSyncCapability::Kind(
        TextDocumentSyncKind::INCREMENTAL,
    ));
    capabilities.semantic_tokens_provider = semantic_tokens_capabilities();

    let mut comp = capabilities.completion_provider.unwrap_or_else(|| CompletionOptions {
        resolve_provider: Some(false),
        ..Default::default()
    });
    let mut triggers = comp.trigger_characters.unwrap_or_default();
    for ch in ["@", "<", "."] {
        if !triggers.iter().any(|t| t == ch) {
            triggers.push(ch.to_string());
        }
    }
    comp.trigger_characters = Some(triggers);
    capabilities.completion_provider = Some(comp);

    capabilities.definition_provider = Some(OneOf::Left(true));
    capabilities.hover_provider = Some(HoverProviderCapability::Simple(true));
    capabilities.workspace = workspace_capabilities();

    capabilities
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_server_capabilities_merging() {
        let mut downstream = ServerCapabilities::default();
        downstream.references_provider = Some(OneOf::Left(true));
        downstream.type_definition_provider =
            Some(tower_lsp::lsp_types::TypeDefinitionProviderCapability::Simple(true));
        downstream.completion_provider = Some(CompletionOptions {
            trigger_characters: Some(vec![":".to_string()]),
            ..Default::default()
        });

        let composite = build_server_capabilities(Some(downstream));

        // Downstream capabilities preserved
        assert_eq!(composite.references_provider, Some(OneOf::Left(true)));
        assert!(composite.type_definition_provider.is_some());

        // Rshtml custom capabilities applied
        assert!(composite.semantic_tokens_provider.is_some());
        assert!(composite.definition_provider.is_some());
        assert!(composite.hover_provider.is_some());

        // Trigger characters merged with @, <, .
        let triggers = composite
            .completion_provider
            .and_then(|c| c.trigger_characters)
            .expect("Trigger characters must be present");
        assert!(triggers.contains(&":".to_string()));
        assert!(triggers.contains(&"@".to_string()));
        assert!(triggers.contains(&"<".to_string()));
        assert!(triggers.contains(&".".to_string()));
    }
}
