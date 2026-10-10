// An HTML parser: the WHATWG tokenizer (§13.2.5) in `tokenizer`, and tree
// construction (§13.2.6) in `tree`, with insertion modes, the stack of
// open elements, active formatting elements, the adoption agency
// algorithm, implied end tags and foster parenting.
//
// A real one, because the cases a shortcut gets wrong fall out of these
// algorithms: `<p>1<b>2<i>3</b>4</i>5` splits the `<i>` across two
// parents, `<table><b>x</b></table>` moves the `<b>` before the table,
// `<li>a<li>b` closes the first item.
//
// The tree is an arena (`Vec<Node>` and indices), since the parser walks
// up from nodes, holds several cursors into the tree, and reparents
// subtrees; indices make all three ordinary.
//
// The markdown renderer hands it raw HTML as a fragment
// (`parse_fragment`), so a preview renders what the markup means. No
// script execution, `document.write` or encoding sniffing (input is a
// decoded `&str`); `<template>` contents go into the template element
// itself, a distinction only script would see.

// The spec's whole surface (every node kind, namespace, quirks mode,
// parse error) is kept though callers use a subset, so the next caller
// needing HTML doesn't reopen it; hence the allow.
#![allow(dead_code)]

pub mod entities;
pub mod tokenizer;
pub mod tree;

pub use tokenizer::Attr;

pub type NodeId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    Html,
    MathMl,
    Svg,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeData {
    Document,
    Doctype { name: String, public_id: String, system_id: String },
    Element { name: String, ns: Namespace, attrs: Vec<Attr> },
    Text(String),
    Comment(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub data: NodeData,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
}

impl Node {
    pub fn name(&self) -> Option<&str> {
        match &self.data {
            NodeData::Element { name, .. } => Some(name),
            _ => None,
        }
    }

    pub fn attr(&self, want: &str) -> Option<&str> {
        match &self.data {
            NodeData::Element { attrs, .. } => attrs.iter().find(|a| a.name == want).map(|a| a.value.as_str()),
            _ => None,
        }
    }
}

// What the doctype, or its absence, said about rendering. Unused, since
// nothing here lays out CSS, but it's what the parse produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuirksMode {
    NoQuirks,
    LimitedQuirks,
    Quirks,
}

#[derive(Debug, Clone)]
pub struct Document {
    pub nodes: Vec<Node>,
    pub root: NodeId,
    pub quirks: QuirksMode,
    // Every parse error the spec names, in order. HTML has no fatal errors,
    // each has a defined recovery, but a linter or preview may want to say
    // the markup is malformed.
    pub errors: Vec<String>,
}

impl Document {
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id].children
    }

    // Every descendant text node concatenated, the DOM's `textContent`: what
    // a terminal renderer shows for an element it has nothing better for.
    pub fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.collect_text(id, &mut out);
        out
    }

    fn collect_text(&self, id: NodeId, out: &mut String) {
        match &self.nodes[id].data {
            NodeData::Text(t) => out.push_str(t),
            _ => {
                for &child in &self.nodes[id].children {
                    self.collect_text(child, out);
                }
            }
        }
    }

    // The first element with this tag name, depth first: enough for
    // `<title>` or `<body>` without a selector engine.
    pub fn find(&self, name: &str) -> Option<NodeId> {
        self.find_from(self.root, name)
    }

    fn find_from(&self, id: NodeId, name: &str) -> Option<NodeId> {
        if self.nodes[id].name() == Some(name) {
            return Some(id);
        }
        self.nodes[id].children.iter().find_map(|&c| self.find_from(c, name))
    }

    // The tree in html5lib-tests' indented format, so a tree assertion reads
    // as the tree:
    //
    //     | <html>
    //     |   <head>
    //     |   <body>
    //     |     "text"
    pub fn to_test_format(&self) -> String {
        let mut out = String::new();
        for &child in &self.nodes[self.root].children {
            self.write_test_format(child, 0, &mut out);
        }
        out
    }

    fn write_test_format(&self, id: NodeId, depth: usize, out: &mut String) {
        // One space after the bar, then two per level, as html5lib-tests
        // has it.
        let indent = format!(" {}", "  ".repeat(depth));
        match &self.nodes[id].data {
            NodeData::Document => {}
            NodeData::Doctype { name, public_id, system_id } => {
                if public_id.is_empty() && system_id.is_empty() {
                    out.push_str(&format!("|{indent}<!DOCTYPE {name}>\n"));
                } else {
                    out.push_str(&format!("|{indent}<!DOCTYPE {name} \"{public_id}\" \"{system_id}\">\n"));
                }
            }
            NodeData::Element { name, ns, attrs } => {
                let prefix = match ns {
                    Namespace::Html => "",
                    Namespace::Svg => "svg ",
                    Namespace::MathMl => "math ",
                };
                out.push_str(&format!("|{indent}<{prefix}{name}>\n"));
                // Attributes sorted, as html5lib-tests prints them.
                let mut sorted: Vec<&Attr> = attrs.iter().collect();
                crate::sort::sort_by(&mut sorted, |a, b| a.name.cmp(&b.name));
                for attr in sorted {
                    out.push_str(&format!("|{indent}  {}=\"{}\"\n", attr.name, attr.value));
                }
            }
            NodeData::Text(t) => out.push_str(&format!("|{indent}\"{t}\"\n")),
            NodeData::Comment(c) => out.push_str(&format!("|{indent}<!-- {c} -->\n")),
        }
        for &child in &self.nodes[id].children {
            self.write_test_format(id_or(child), depth + 1, out);
        }
    }
}

fn id_or(id: NodeId) -> NodeId {
    id
}

// A whole document, inserting the `<html>`, `<head>` and `<body>` a
// document may omit.
pub fn parse(input: &str) -> Document {
    tree::TreeBuilder::new(None).run(input)
}

// A fragment parsed as the contents of `context`, as markdown needs: a
// `<td>` context parses `<tr>` very differently from a `<div>`. Returns
// the document that owns the nodes and the fragment's top-level ones.
pub fn parse_fragment(input: &str, context: &str) -> (Document, Vec<NodeId>) {
    let doc = tree::TreeBuilder::new(Some(context.to_string())).run(input);
    // A fragment parses into a synthetic `<html>` root whose children are
    // the fragment (§13.2.6.5).
    let root_children = match doc.nodes[doc.root].children.first() {
        Some(&html) => doc.nodes[html].children.clone(),
        None => Vec::new(),
    };
    (doc, root_children)
}
