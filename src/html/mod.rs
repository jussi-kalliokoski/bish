// A real HTML parser: the WHATWG tokenizer (§13.2.5) in `tokenizer`, and
// tree construction (§13.2.6) in `tree` -- the insertion-mode state
// machine, the stack of open elements, the list of active formatting
// elements, the adoption agency algorithm, implied end tags and foster
// parenting. No external crate, same as every other parser here.
//
// Why a *real* one rather than "find the tags": the whole value is in
// what a shortcut gets wrong. `<p>1<b>2<i>3</b>4</i>5` has to come out
// with the `<i>` split across two parents; `<table><b>x</b></table>` has
// to move the `<b>` out in front of the table; `<li>a<li>b` has to close
// the first item without being told to. Those all fall out of the
// algorithms below rather than being special cases, which is the point:
// nothing here has to guess what an author meant.
//
// The tree is an arena -- `Vec<Node>` plus indices -- rather than
// `Rc<RefCell<Node>>`. The parser needs to walk *up* from a node, hold
// several cursors into the tree at once (the stack of open elements and
// the formatting list both point into it), and reparent subtrees during
// the adoption agency algorithm; indices make all three ordinary
// operations and keep the whole document one allocation to hand around.
//
// Called by markdown.rs, which hands over the raw HTML it finds in a
// document (as a fragment, per `parse_fragment`) so a preview can render
// what the markup actually means rather than dropping it.
//
// Deliberately absent, because there is no scripting host and nothing to
// render into: script execution, `document.write` (which is what would
// force the tokenizer to be re-entrant), and encoding sniffing (input is
// already-decoded Rust `&str`). `<template>` contents are parsed into
// the template element itself rather than into a separate document
// fragment -- the distinction only matters to script.

// A spec-complete parser exposes the spec's own surface -- every node
// kind, every namespace, the quirks mode, the parse-error list -- and
// this codebase's own callers (markdown.rs, and a preview through it)
// use a subset of it. Kept whole rather than trimmed to today's callers:
// the value of following the spec is that the next thing to need HTML
// doesn't have to reopen it, and a parser missing the parts nobody
// happened to call yet is exactly the shortcut this exists not to be.
// Same reasoning, and same allow, as bishedit::highlight's own.
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

// What the doctype (or the lack of one) said about how to render. Not
// acted on here -- nothing in this codebase lays out CSS -- but it is
// what the parse produced, and dropping it would mean this couldn't
// report on a document it was asked about.
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
    // Every parse error the spec names, in the order they happened.
    // Collected rather than acted on: HTML has no fatal parse error --
    // every one has defined recovery -- but something has to be able to
    // *report* them for a linter or a preview to say "this markup is
    // malformed" without changing what it renders.
    pub errors: Vec<String>,
}

impl Document {
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id].children
    }

    // Every descendant text node, concatenated -- the DOM's own
    // `textContent`, and what a terminal renderer wants from an element
    // it has no better idea what to do with.
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

    // The first element with this tag name, depth first -- enough for
    // `<title>`, `<body>` and the like without a selector engine.
    pub fn find(&self, name: &str) -> Option<NodeId> {
        self.find_from(self.root, name)
    }

    fn find_from(&self, id: NodeId, name: &str) -> Option<NodeId> {
        if self.nodes[id].name() == Some(name) {
            return Some(id);
        }
        self.nodes[id].children.iter().find_map(|&c| self.find_from(c, name))
    }

    // A debugging/testing view of the tree in html5lib-tests' own
    // indented format, which is what makes a tree-shape assertion
    // readable as the tree it describes:
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
        // One space after the bar, then two per level -- html5lib-tests'
        // own spacing, so a tree here can be diffed against theirs.
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
                // Attributes are printed sorted, as html5lib-tests does,
                // so a tree comparison doesn't depend on source order.
                let mut sorted: Vec<&Attr> = attrs.iter().collect();
                sorted.sort_by(|a, b| a.name.cmp(&b.name));
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

// A whole document: everything the spec does, including inserting the
// `<html>`/`<head>`/`<body>` a document may never have named.
pub fn parse(input: &str) -> Document {
    tree::TreeBuilder::new(None).run(input)
}

// A fragment parsed as if it were the contents of `context` -- what
// markdown needs, since HTML in a markdown document is a snippet in some
// surrounding element rather than a document of its own. `context`
// decides the rules that apply: a `<td>` context parses `<tr>` very
// differently from a `<div>` one.
//
// Returns the fragment's own top-level nodes plus the document that owns
// them, since nodes are arena indices rather than owning values.
pub fn parse_fragment(input: &str, context: &str) -> (Document, Vec<NodeId>) {
    let doc = tree::TreeBuilder::new(Some(context.to_string())).run(input);
    // The fragment case parses into a synthetic `<html>` root whose
    // children are the fragment (§13.2.6.5), so that's what to hand back.
    let root_children = match doc.nodes[doc.root].children.first() {
        Some(&html) => doc.nodes[html].children.clone(),
        None => Vec::new(),
    };
    (doc, root_children)
}
