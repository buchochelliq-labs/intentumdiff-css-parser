//! CSS parser plugin - full-parse mode.
//!
//! Handles `.css` files.
//! Parses source with tree-sitter-css directly.

use intentumdiff_plugin_sdk::{
    cst::CstNode,
    hash::structural_hash_with_memo,
    tree::{SemanticNode, SemanticNodeBuilder},
};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentumdiff::plugin::parser::ExamplePair;
use crate::exports::intentumdiff::plugin::parser::Guest;
use crate::exports::intentumdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentumdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct CssParser;

const TRIVIA: &[&str] = &["comment", "whitespace"];

const SEMANTIC_TYPES: &[&str] = &[
    // Root
    "stylesheet",
    // Rule set: selector(s) + block of declarations
    "rule_set",
    // Declarations inside a block
    "declaration",
    // At-rules
    "media_statement",
    "keyframes_statement",
    "keyframe_block",
    "import_statement",
    "supports_statement",
    "charset_statement",
    "namespace_statement",
    "at_rule",
    // Selectors — each selector variant is a meaningful diff unit
    "class_selector",
    "id_selector",
    "type_selector",
    "pseudo_class_selector",
    "pseudo_element_selector",
    "attribute_selector",
    "universal_selector",
    "child_selector",
    "sibling_selector",
    "adjacent_sibling_selector",
    "descendant_selector",
    "selectors",
];

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

/// Return a human-readable label for a CSS node.
fn label_for(node: &CstNode) -> String {
    if node.is_leaf() {
        return node.text_or_empty().to_string();
    }
    // Literal containers label with their captured source text (SDK-shared, issue #47).
    if let Some(label) = intentumdiff_plugin_sdk::ts_convert::literal_label(node) {
        return label;
    }
    match node.node_type.as_str() {
        "rule_set" => {
            // First named child is the selectors
            for child in &node.children {
                if child.node_type == "selectors" {
                    return selector_text(child);
                }
            }
        }
        "declaration" => {
            for child in &node.children {
                if child.node_type == "property_name" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "media_statement" => {
            // Return the first keyword_query or feature_query text as label
            for child in &node.children {
                if matches!(
                    child.node_type.as_str(),
                    "keyword_query" | "feature_query" | "binary_query" | "media_query"
                ) {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "keyframes_statement" => {
            for child in &node.children {
                if child.node_type == "keyframes_name" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "import_statement" => {
            for child in &node.children {
                if matches!(child.node_type.as_str(), "string_value" | "call_expression") {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "class_selector" => {
            // Child: "." (unnamed) + identifier (named)
            for child in &node.children {
                if child.node_type == "class_name" || child.is_leaf() {
                    let t = child.text_or_empty();
                    if !t.is_empty() && t != "." {
                        return format!(".{}", t);
                    }
                }
            }
        }
        "id_selector" => {
            for child in &node.children {
                if child.node_type == "id_name" || child.is_leaf() {
                    let t = child.text_or_empty();
                    if !t.is_empty() && t != "#" {
                        return format!("#{}", t);
                    }
                }
            }
        }
        "type_selector" | "tag_name" => {
            // The node itself may be the leaf in some grammar versions
            for child in &node.children {
                if child.is_leaf() {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "pseudo_class_selector" => {
            // ":hover", ":nth-child(2n)"
            for child in &node.children {
                if child.node_type == "class_name" || child.is_leaf() {
                    let t = child.text_or_empty();
                    if !t.is_empty() && t != ":" {
                        return format!(":{}", t);
                    }
                }
            }
        }
        "pseudo_element_selector" => {
            for child in &node.children {
                if child.is_leaf() {
                    let t = child.text_or_empty();
                    if !t.is_empty() && t != ":" {
                        return format!("::{}", t);
                    }
                }
            }
        }
        _ => {}
    }
    // Generic fallback: first leaf text
    for child in &node.children {
        if child.is_leaf() {
            let t = child.text_or_empty();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    node.node_type.clone()
}

/// Produce a compact text representation of a selectors node.
fn selector_text(node: &CstNode) -> String {
    if node.is_leaf() {
        return node.text_or_empty().to_string();
    }
    let parts: Vec<String> = node
        .children
        .iter()
        .map(|c| {
            if c.is_leaf() {
                c.text_or_empty().to_string()
            } else {
                label_for(c)
            }
        })
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        node.node_type.clone()
    } else {
        parts.join(", ")
    }
}

fn convert(
    node: &CstNode,
    id_prefix: &str,
    memo: &mut std::collections::HashMap<usize, String>,
) -> Option<SemanticNode> {
    convert_semantic_strict(
        node,
        id_prefix,
        memo,
        &|t| TRIVIA.contains(&t),
        &is_semantic,
        &label_for,
    )
}



use intentumdiff_plugin_sdk::ts_convert::{convert_semantic_strict, node_to_cst};

fn parse_source(source: &str) -> Result<CstNode, String> {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_css::LANGUAGE.into();
    parser
        .set_language(&lang)
        .map_err(|_| "Failed to load CSS grammar".to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "Parse failed".to_string())?;
    Ok(node_to_cst(tree.root_node(), source.as_bytes()))
}

fn process_impl(source: &str) -> String {
    let root: CstNode = match parse_source(source) {
        Ok(n) => n,
        Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
    };
    let mut memo: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let sem = match convert(&root, "0", &mut memo) {
        Some(n) => n,
        None => return r#"{"error":"Empty semantic tree"}"#.to_string(),
    };
    match serde_json::to_string(&sem) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for CssParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "css".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        if filename.to_lowercase().ends_with(".css") {
            return "css".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: ".button {\n  background: blue;\n  color: white;\n  padding: 10px;\n  border: none;\n}\n\n.button:hover {\n  background: darkblue;\n}\n".to_string(),
            new: ".button {\n  background-color: #2563eb;\n  color: #ffffff;\n  padding: 8px 16px;\n  border: none;\n  border-radius: 6px;\n  font-size: 14px;\n  cursor: pointer;\n  transition: background-color 0.2s ease;\n}\n\n.button:hover {\n  background-color: #1d4ed8;\n}\n\n.button:focus {\n  outline: 2px solid #93c5fd;\n  outline-offset: 2px;\n}\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["css".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(CssParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentumdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!CssParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = CssParser::grammar_id();
        let ids = CssParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_css() {
        assert_eq!(
            CssParser::detect_language("style.css".to_string(), "".to_string()),
            "css"
        );
    }

    #[test]
    fn detect_language_unknown() {
        let r = CssParser::detect_language("main.rs".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }
}
