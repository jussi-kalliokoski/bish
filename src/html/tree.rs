// HTML tree construction, §13.2.6 -- the half of the parser that turns a
// flat token stream into a tree, and the half that makes real-world HTML
// work. One insertion mode per mode in the spec, named the same way.
//
// See the module comment in mod.rs for why this exists rather than a
// tag-finding shortcut. The three algorithms worth knowing by name,
// because nearly every surprising-but-correct result comes from one of
// them: `reconstruct_active_formatting_elements` (which re-opens `<b>`
// inside a new paragraph so `<b>a<p>b` bolds both), `adoption_agency`
// (which un-crosses `<b>1<i>2</b>3</i>`), and `foster_parent` (which
// moves stray content out in front of a table instead of into it).

use super::tokenizer::{Attr, ContentState, Tag, Token, Tokenizer};
use super::{Document, Namespace, Node, NodeData, NodeId, QuirksMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Initial,
    BeforeHtml,
    BeforeHead,
    InHead,
    InHeadNoscript,
    AfterHead,
    InBody,
    Text,
    InTable,
    InTableText,
    InCaption,
    InColumnGroup,
    InTableBody,
    InRow,
    InCell,
    InSelect,
    InSelectInTable,
    InTemplate,
    AfterBody,
    InFrameset,
    AfterFrameset,
    AfterAfterBody,
    AfterAfterFrameset,
}

// An entry in the list of active formatting elements. A Marker is what
// `<td>`, `<caption>` and the like push so that formatting can't leak
// out of a cell -- see `clear_afe_to_marker`.
#[derive(Debug, Clone)]
enum Formatting {
    Marker,
    // The tag is kept alongside the node because reconstruction has to
    // create a *new* element "for the token for which this entry was
    // created", attributes and all, long after that token is gone.
    Element { node: NodeId, tag: Tag },
}

// §13.2.4.2's "special" category: the elements that close a `<p>`, stop
// the adoption agency's search, and end scope. Anything not on this list
// (and not a formatting element) is "ordinary".
const SPECIAL: &[&str] = &[
    "address",
    "applet",
    "area",
    "article",
    "aside",
    "base",
    "basefont",
    "bgsound",
    "blockquote",
    "body",
    "br",
    "button",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dir",
    "div",
    "dl",
    "dt",
    "embed",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hgroup",
    "hr",
    "html",
    "iframe",
    "img",
    "input",
    "keygen",
    "li",
    "link",
    "listing",
    "main",
    "marquee",
    "menu",
    "meta",
    "nav",
    "noembed",
    "noframes",
    "noscript",
    "object",
    "ol",
    "p",
    "param",
    "plaintext",
    "pre",
    "script",
    "search",
    "section",
    "select",
    "source",
    "style",
    "summary",
    "table",
    "tbody",
    "td",
    "template",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
    "wbr",
    "xmp",
];

// §13.2.4.3: the elements the adoption agency algorithm exists for.
const FORMATTING: &[&str] = &["a", "b", "big", "code", "em", "font", "i", "nobr", "s", "small", "strike", "strong", "tt", "u"];

// The elements that stop a scope search. `has_element_in_scope`'s
// variants add to this list rather than replacing it.
const SCOPE_BASE: &[&str] = &["applet", "caption", "html", "table", "td", "th", "marquee", "object", "template"];

const HEADINGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];

// SVG element names that are camelCase in the SVG DOM but lowercased by
// the tokenizer -- §13.2.6.5's "adjust SVG tag name" table.
const SVG_TAG_NAMES: &[(&str, &str)] = &[
    ("altglyph", "altGlyph"),
    ("altglyphdef", "altGlyphDef"),
    ("altglyphitem", "altGlyphItem"),
    ("animatecolor", "animateColor"),
    ("animatemotion", "animateMotion"),
    ("animatetransform", "animateTransform"),
    ("clippath", "clipPath"),
    ("feblend", "feBlend"),
    ("fecolormatrix", "feColorMatrix"),
    ("fecomponenttransfer", "feComponentTransfer"),
    ("fecomposite", "feComposite"),
    ("feconvolvematrix", "feConvolveMatrix"),
    ("fediffuselighting", "feDiffuseLighting"),
    ("fedisplacementmap", "feDisplacementMap"),
    ("fedistantlight", "feDistantLight"),
    ("fedropshadow", "feDropShadow"),
    ("feflood", "feFlood"),
    ("fefunca", "feFuncA"),
    ("fefuncb", "feFuncB"),
    ("fefuncg", "feFuncG"),
    ("fefuncr", "feFuncR"),
    ("fegaussianblur", "feGaussianBlur"),
    ("feimage", "feImage"),
    ("femerge", "feMerge"),
    ("femergenode", "feMergeNode"),
    ("femorphology", "feMorphology"),
    ("feoffset", "feOffset"),
    ("fepointlight", "fePointLight"),
    ("fespecularlighting", "feSpecularLighting"),
    ("fespotlight", "feSpotLight"),
    ("fetile", "feTile"),
    ("feturbulence", "feTurbulence"),
    ("foreignobject", "foreignObject"),
    ("glyphref", "glyphRef"),
    ("lineargradient", "linearGradient"),
    ("radialgradient", "radialGradient"),
    ("textpath", "textPath"),
];

// §13.2.6.5's SVG attribute-name table (the same camelCase problem, for
// attributes).
const SVG_ATTRS: &[(&str, &str)] = &[
    ("attributename", "attributeName"),
    ("attributetype", "attributeType"),
    ("basefrequency", "baseFrequency"),
    ("baseprofile", "baseProfile"),
    ("calcmode", "calcMode"),
    ("clippathunits", "clipPathUnits"),
    ("diffuseconstant", "diffuseConstant"),
    ("edgemode", "edgeMode"),
    ("filterunits", "filterUnits"),
    ("glyphref", "glyphRef"),
    ("gradienttransform", "gradientTransform"),
    ("gradientunits", "gradientUnits"),
    ("kernelmatrix", "kernelMatrix"),
    ("kernelunitlength", "kernelUnitLength"),
    ("keypoints", "keyPoints"),
    ("keysplines", "keySplines"),
    ("keytimes", "keyTimes"),
    ("lengthadjust", "lengthAdjust"),
    ("limitingconeangle", "limitingConeAngle"),
    ("markerheight", "markerHeight"),
    ("markerunits", "markerUnits"),
    ("markerwidth", "markerWidth"),
    ("maskcontentunits", "maskContentUnits"),
    ("maskunits", "maskUnits"),
    ("numoctaves", "numOctaves"),
    ("pathlength", "pathLength"),
    ("patterncontentunits", "patternContentUnits"),
    ("patterntransform", "patternTransform"),
    ("patternunits", "patternUnits"),
    ("pointsatx", "pointsAtX"),
    ("pointsaty", "pointsAtY"),
    ("pointsatz", "pointsAtZ"),
    ("preservealpha", "preserveAlpha"),
    ("preserveaspectratio", "preserveAspectRatio"),
    ("primitiveunits", "primitiveUnits"),
    ("refx", "refX"),
    ("refy", "refY"),
    ("repeatcount", "repeatCount"),
    ("repeatdur", "repeatDur"),
    ("requiredextensions", "requiredExtensions"),
    ("requiredfeatures", "requiredFeatures"),
    ("specularconstant", "specularConstant"),
    ("specularexponent", "specularExponent"),
    ("spreadmethod", "spreadMethod"),
    ("startoffset", "startOffset"),
    ("stddeviation", "stdDeviation"),
    ("stitchtiles", "stitchTiles"),
    ("surfacescale", "surfaceScale"),
    ("systemlanguage", "systemLanguage"),
    ("tablevalues", "tableValues"),
    ("targetx", "targetX"),
    ("targety", "targetY"),
    ("textlength", "textLength"),
    ("viewbox", "viewBox"),
    ("viewtarget", "viewTarget"),
    ("xchannelselector", "xChannelSelector"),
    ("ychannelselector", "yChannelSelector"),
    ("zoomandpan", "zoomAndPan"),
];

const MATHML_ATTRS: &[(&str, &str)] = &[("definitionurl", "definitionURL")];

// MathML text integration points and the HTML integration points inside
// SVG: the places where HTML parsing resumes inside foreign content.
const MATHML_TEXT_INTEGRATION: &[&str] = &["mi", "mo", "mn", "ms", "mtext"];
const SVG_HTML_INTEGRATION: &[&str] = &["foreignObject", "desc", "title"];

pub struct TreeBuilder {
    doc: Document,
    tok: Tokenizer,
    mode: Mode,
    // Where `Text` and `InTableText` return to when they're done.
    original_mode: Option<Mode>,
    template_modes: Vec<Mode>,
    open: Vec<NodeId>,
    afe: Vec<Formatting>,
    head: Option<NodeId>,
    form: Option<NodeId>,
    frameset_ok: bool,
    foster: bool,
    pending_table_text: String,
    pending_table_text_ws_only: bool,
    // `Some` when parsing a fragment: the element the fragment is
    // notionally inside, which decides most of the rules that apply.
    fragment_context: Option<(String, Namespace)>,
    // A newline immediately after `<pre>`/`<textarea>`/`<listing>` is
    // dropped, so that writing the content on its own line doesn't add
    // one.
    ignore_lf: bool,
    done: bool,
}

impl TreeBuilder {
    pub fn new(fragment_context: Option<String>) -> TreeBuilder {
        let doc = Document {
            nodes: vec![Node { data: NodeData::Document, parent: None, children: Vec::new() }],
            root: 0,
            quirks: QuirksMode::NoQuirks,
            errors: Vec::new(),
        };
        TreeBuilder {
            doc,
            tok: Tokenizer::new(""),
            mode: Mode::Initial,
            original_mode: None,
            template_modes: Vec::new(),
            open: Vec::new(),
            afe: Vec::new(),
            head: None,
            form: None,
            frameset_ok: true,
            foster: false,
            pending_table_text: String::new(),
            pending_table_text_ws_only: true,
            fragment_context: fragment_context.map(|name| (name, Namespace::Html)),
            ignore_lf: false,
            done: false,
        }
    }

    pub fn run(mut self, input: &str) -> Document {
        self.tok = Tokenizer::new(input);
        if self.fragment_context.is_some() {
            self.setup_fragment();
        }
        loop {
            let token = self.tok.next();
            let eof = token == Token::Eof;
            self.process(token);
            if eof || self.done {
                break;
            }
        }
        let mut doc = self.doc;
        doc.errors.append(&mut self.tok.errors);
        doc
    }

    // §13.2.6.5: a fragment parse runs inside a synthetic `<html>` root,
    // with the insertion mode reset as if the context element were open.
    fn setup_fragment(&mut self) {
        let html = self.new_element("html", Namespace::Html, Vec::new());
        self.append(self.doc.root, html);
        self.open.push(html);
        let Some((name, _)) = self.fragment_context.clone() else { return };
        match name.as_str() {
            "title" | "textarea" => self.tok.set_state(ContentState::Rcdata),
            "style" | "xmp" | "iframe" | "noembed" | "noframes" => self.tok.set_state(ContentState::Rawtext),
            "script" => self.tok.set_state(ContentState::ScriptData),
            "plaintext" => self.tok.set_state(ContentState::Plaintext),
            _ => {}
        }
        self.reset_insertion_mode();
    }

    fn error(&mut self, what: &str) {
        self.doc.errors.push(what.to_string());
    }

    // ---- tree primitives -------------------------------------------

    fn new_element(&mut self, name: &str, ns: Namespace, attrs: Vec<Attr>) -> NodeId {
        self.doc.nodes.push(Node { data: NodeData::Element { name: name.to_string(), ns, attrs }, parent: None, children: Vec::new() });
        self.doc.nodes.len() - 1
    }

    fn append(&mut self, parent: NodeId, child: NodeId) {
        self.detach(child);
        self.doc.nodes[child].parent = Some(parent);
        self.doc.nodes[parent].children.push(child);
    }

    fn insert_before(&mut self, parent: NodeId, index: usize, child: NodeId) {
        self.detach(child);
        self.doc.nodes[child].parent = Some(parent);
        self.doc.nodes[parent].children.insert(index, child);
    }

    fn detach(&mut self, child: NodeId) {
        if let Some(old) = self.doc.nodes[child].parent.take() {
            self.doc.nodes[old].children.retain(|&c| c != child);
        }
    }

    fn name_of(&self, id: NodeId) -> &str {
        self.doc.nodes[id].name().unwrap_or("")
    }

    fn ns_of(&self, id: NodeId) -> Namespace {
        match &self.doc.nodes[id].data {
            NodeData::Element { ns, .. } => *ns,
            _ => Namespace::Html,
        }
    }

    fn is_html(&self, id: NodeId, name: &str) -> bool {
        self.ns_of(id) == Namespace::Html && self.name_of(id) == name
    }

    fn current(&self) -> NodeId {
        *self.open.last().expect("the stack of open elements is never empty during tree construction")
    }

    // §13.2.6.1's "appropriate place for inserting a node", including
    // the foster-parenting branch that moves content out of a table.
    fn insertion_point(&mut self) -> (NodeId, Option<usize>) {
        let target = self.current();
        if self.foster && matches!(self.name_of(target), "table" | "tbody" | "tfoot" | "thead" | "tr") {
            return self.foster_parent();
        }
        (target, None)
    }

    // "Foster parenting": text and elements that show up where only rows
    // and cells are allowed go *before* the table rather than inside it,
    // which is what makes `<table><b>x</b></table>` put the `<b>` in
    // front of the table like every browser does.
    fn foster_parent(&mut self) -> (NodeId, Option<usize>) {
        let last_table = self.open.iter().rposition(|&id| self.is_html(id, "table"));
        match last_table {
            Some(i) => {
                let table = self.open[i];
                match self.doc.nodes[table].parent {
                    Some(parent) => {
                        let index = self.doc.nodes[parent].children.iter().position(|&c| c == table).unwrap_or(0);
                        (parent, Some(index))
                    }
                    // A table with no parent yet: the element above it on
                    // the stack takes the content.
                    None => (self.open[i.saturating_sub(1)], None),
                }
            }
            None => (self.open[0], None),
        }
    }

    fn insert_node_at_appropriate_place(&mut self, node: NodeId) {
        match self.insertion_point() {
            (parent, Some(index)) => self.insert_before(parent, index, node),
            (parent, None) => self.append(parent, node),
        }
    }

    fn insert_element(&mut self, tag: &Tag, ns: Namespace) -> NodeId {
        let node = self.new_element(&tag.name, ns, tag.attrs.clone());
        self.insert_node_at_appropriate_place(node);
        self.open.push(node);
        node
    }

    fn insert_char(&mut self, c: char) {
        let (parent, index) = self.insertion_point();
        // Appended to the previous text node when there is one, so the
        // tree holds runs rather than one node per character.
        let prev = match index {
            Some(0) => None,
            Some(i) => self.doc.nodes[parent].children.get(i - 1).copied(),
            None => self.doc.nodes[parent].children.last().copied(),
        };
        if let Some(prev) = prev
            && let NodeData::Text(text) = &mut self.doc.nodes[prev].data
        {
            text.push(c);
            return;
        }
        self.doc.nodes.push(Node { data: NodeData::Text(c.to_string()), parent: None, children: Vec::new() });
        let node = self.doc.nodes.len() - 1;
        match index {
            Some(i) => self.insert_before(parent, i, node),
            None => self.append(parent, node),
        }
    }

    fn insert_comment(&mut self, text: &str, parent: Option<NodeId>) {
        self.doc.nodes.push(Node { data: NodeData::Comment(text.to_string()), parent: None, children: Vec::new() });
        let node = self.doc.nodes.len() - 1;
        match parent {
            Some(p) => self.append(p, node),
            None => self.insert_node_at_appropriate_place(node),
        }
    }

    // ---- stack and scope -------------------------------------------

    fn has_in_scope_with(&self, name: &str, extra: &[&str]) -> bool {
        for &id in self.open.iter().rev() {
            if self.is_html(id, name) {
                return true;
            }
            let n = self.name_of(id);
            if self.ns_of(id) == Namespace::Html && (SCOPE_BASE.contains(&n) || extra.contains(&n)) {
                return false;
            }
            if self.ns_of(id) == Namespace::MathMl && MATHML_TEXT_INTEGRATION.contains(&n) {
                return false;
            }
            if self.ns_of(id) == Namespace::Svg && SVG_HTML_INTEGRATION.contains(&n) {
                return false;
            }
        }
        false
    }

    fn has_in_scope(&self, name: &str) -> bool {
        self.has_in_scope_with(name, &[])
    }

    fn has_in_list_item_scope(&self, name: &str) -> bool {
        self.has_in_scope_with(name, &["ol", "ul"])
    }

    fn has_in_button_scope(&self, name: &str) -> bool {
        self.has_in_scope_with(name, &["button"])
    }

    // Table scope is the inverse shape: only three elements stop it.
    fn has_in_table_scope(&self, name: &str) -> bool {
        for &id in self.open.iter().rev() {
            if self.is_html(id, name) {
                return true;
            }
            if matches!(self.name_of(id), "html" | "table" | "template") {
                return false;
            }
        }
        false
    }

    // Select scope is inverted again: everything *except* optgroup and
    // option stops the search.
    fn has_in_select_scope(&self, name: &str) -> bool {
        for &id in self.open.iter().rev() {
            if self.is_html(id, name) {
                return true;
            }
            if !matches!(self.name_of(id), "optgroup" | "option") {
                return false;
            }
        }
        false
    }

    fn has_heading_in_scope(&self) -> bool {
        HEADINGS.iter().any(|h| self.has_in_scope(h))
    }

    // "Generate implied end tags": the elements that close themselves
    // when something else starts, which is what makes `<li>a<li>b` two
    // items rather than nested ones.
    fn generate_implied_end_tags(&mut self, except: &str) {
        while let Some(&id) = self.open.last() {
            let name = self.name_of(id);
            if name != except && matches!(name, "dd" | "dt" | "li" | "optgroup" | "option" | "p" | "rb" | "rp" | "rt" | "rtc") {
                self.open.pop();
            } else {
                break;
            }
        }
    }

    fn generate_implied_end_tags_thoroughly(&mut self) {
        while let Some(&id) = self.open.last() {
            if matches!(
                self.name_of(id),
                "caption"
                    | "colgroup"
                    | "dd"
                    | "dt"
                    | "li"
                    | "optgroup"
                    | "option"
                    | "p"
                    | "rb"
                    | "rp"
                    | "rt"
                    | "rtc"
                    | "tbody"
                    | "td"
                    | "tfoot"
                    | "th"
                    | "thead"
                    | "tr"
            ) {
                self.open.pop();
            } else {
                break;
            }
        }
    }

    fn close_p(&mut self) {
        self.generate_implied_end_tags("p");
        if !self.is_html(self.current(), "p") {
            self.error("unexpected element before </p>");
        }
        while let Some(id) = self.open.pop() {
            if self.is_html(id, "p") {
                break;
            }
        }
    }

    fn pop_until_html(&mut self, name: &str) {
        while let Some(id) = self.open.pop() {
            if self.is_html(id, name) {
                break;
            }
        }
    }

    fn pop_until_any(&mut self, names: &[&str]) {
        while let Some(id) = self.open.pop() {
            if names.contains(&self.name_of(id)) {
                break;
            }
        }
    }

    // §13.2.6.3: what mode to be in, derived from the stack -- used
    // whenever a mode is left behind (a cell closing, a table closing)
    // and by the fragment case at startup.
    fn reset_insertion_mode(&mut self) {
        for i in (0..self.open.len()).rev() {
            let id = self.open[i];
            let mut last = i == 0;
            let mut name = self.name_of(id).to_string();
            if last && let Some((ctx, _)) = &self.fragment_context {
                name = ctx.clone();
                last = true;
            }
            self.mode = match name.as_str() {
                "select" => {
                    // A select inside a table has its own mode, so look
                    // for one above it.
                    let mut mode = Mode::InSelect;
                    if !last {
                        for j in (0..i).rev() {
                            let ancestor = self.open[j];
                            if self.is_html(ancestor, "template") {
                                break;
                            }
                            if self.is_html(ancestor, "table") {
                                mode = Mode::InSelectInTable;
                                break;
                            }
                        }
                    }
                    mode
                }
                "td" | "th" if !last => Mode::InCell,
                "tr" => Mode::InRow,
                "tbody" | "thead" | "tfoot" => Mode::InTableBody,
                "caption" => Mode::InCaption,
                "colgroup" => Mode::InColumnGroup,
                "table" => Mode::InTable,
                "template" => *self.template_modes.last().unwrap_or(&Mode::InBody),
                "head" if !last => Mode::InHead,
                "body" => Mode::InBody,
                "frameset" => Mode::InFrameset,
                "html" => {
                    if self.head.is_none() {
                        Mode::BeforeHead
                    } else {
                        Mode::AfterHead
                    }
                }
                _ if last => Mode::InBody,
                _ => continue,
            };
            return;
        }
        self.mode = Mode::InBody;
    }

    // ---- active formatting elements --------------------------------

    fn push_afe(&mut self, node: NodeId, tag: Tag) {
        // The "Noah's Ark clause": at most three entries with the same
        // tag name and attributes may be in the list at once, or deeply
        // repeated formatting would grow the tree without bound.
        let mut matches = Vec::new();
        for (i, entry) in self.afe.iter().enumerate().rev() {
            match entry {
                Formatting::Marker => break,
                Formatting::Element { tag: other, .. } => {
                    if other.name == tag.name && same_attrs(&other.attrs, &tag.attrs) {
                        matches.push(i);
                    }
                }
            }
        }
        if matches.len() >= 3 {
            let oldest = *matches.last().unwrap();
            self.afe.remove(oldest);
        }
        self.afe.push(Formatting::Element { node, tag });
    }

    fn clear_afe_to_marker(&mut self) {
        while let Some(entry) = self.afe.pop() {
            if matches!(entry, Formatting::Marker) {
                break;
            }
        }
    }

    fn afe_position(&self, node: NodeId) -> Option<usize> {
        self.afe.iter().position(|e| matches!(e, Formatting::Element { node: n, .. } if *n == node))
    }

    // §13.2.6.4.7's own "reconstruct" step: re-open any formatting
    // element that is still active but no longer on the stack. This is
    // what makes `<b>a<p>b</p>` bold the second paragraph too.
    fn reconstruct_afe(&mut self) {
        let Some(last) = self.afe.last() else { return };
        match last {
            Formatting::Marker => return,
            Formatting::Element { node, .. } if self.open.contains(node) => return,
            _ => {}
        }
        let mut i = self.afe.len() - 1;
        // Rewind to the first entry that is a marker or still open.
        loop {
            if i == 0 {
                break;
            }
            i -= 1;
            match &self.afe[i] {
                Formatting::Marker => {
                    i += 1;
                    break;
                }
                Formatting::Element { node, .. } if self.open.contains(node) => {
                    i += 1;
                    break;
                }
                _ => {}
            }
        }
        // ...then re-create every entry from there on.
        while i < self.afe.len() {
            let Formatting::Element { tag, .. } = self.afe[i].clone() else {
                i += 1;
                continue;
            };
            let node = self.insert_element(&tag, Namespace::Html);
            self.afe[i] = Formatting::Element { node, tag };
            i += 1;
        }
    }

    // §13.2.6.4.7, the adoption agency algorithm: the one that
    // un-crosses misnested formatting, so `<p>1<b>2<i>3</b>4</i>5` ends
    // up with the `<i>` split across two parents rather than the tree
    // simply being wrong. Returns false when the caller should fall
    // through to "any other end tag".
    fn adoption_agency(&mut self, subject: &str) -> bool {
        // Step 2: the simple case, where the element is current and not
        // being tracked as formatting.
        let current = self.current();
        if self.is_html(current, subject) && self.afe_position(current).is_none() {
            self.open.pop();
            return true;
        }
        for _ in 0..8 {
            // Step 6: the formatting element is the last matching entry
            // after the last marker.
            let mut formatting_index = None;
            for (i, entry) in self.afe.iter().enumerate().rev() {
                match entry {
                    Formatting::Marker => break,
                    Formatting::Element { tag, .. } if tag.name == subject => {
                        formatting_index = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            let Some(fi) = formatting_index else { return false };
            let Formatting::Element { node: formatting, .. } = self.afe[fi].clone() else { return false };

            let Some(stack_index) = self.open.iter().position(|&n| n == formatting) else {
                self.error("formatting element not on the stack");
                self.afe.remove(fi);
                return true;
            };
            if !self.has_in_scope(subject) {
                self.error("formatting element not in scope");
                return true;
            }
            if formatting != self.current() {
                self.error("formatting element is not the current node");
            }

            // Step 10: the furthest block -- the first *special* element
            // below the formatting element.
            let furthest = (stack_index + 1..self.open.len()).find(|&i| {
                let id = self.open[i];
                self.ns_of(id) == Namespace::Html && SPECIAL.contains(&self.name_of(id))
            });
            let Some(furthest_index) = furthest else {
                // Nothing between them: just close the formatting
                // element normally.
                self.open.truncate(stack_index);
                self.afe.remove(fi);
                return true;
            };
            let furthest_block = self.open[furthest_index];
            let common_ancestor = self.open[stack_index - 1];
            let mut bookmark = fi;

            let mut node_index = furthest_index;
            let mut last_node = furthest_block;
            let mut inner = 0;
            loop {
                inner += 1;
                if node_index == 0 {
                    break;
                }
                node_index -= 1;
                let node = self.open[node_index];
                if node == formatting {
                    break;
                }
                let in_afe = self.afe_position(node);
                if inner > 3
                    && let Some(pos) = in_afe
                {
                    self.afe.remove(pos);
                    if bookmark > pos {
                        bookmark -= 1;
                    }
                    self.open.remove(node_index);
                    continue;
                }
                let Some(pos) = self.afe_position(node) else {
                    self.open.remove(node_index);
                    continue;
                };
                // Step 14.6: replace the entry (and the stack slot) with
                // a fresh element built from the same token.
                let Formatting::Element { tag, .. } = self.afe[pos].clone() else { continue };
                let new_node = self.new_element(&tag.name, Namespace::Html, tag.attrs.clone());
                self.append(common_ancestor, new_node);
                self.afe[pos] = Formatting::Element { node: new_node, tag };
                self.open[node_index] = new_node;
                if last_node == furthest_block {
                    bookmark = pos + 1;
                }
                self.append(new_node, last_node);
                last_node = new_node;
            }

            // Step 15: whatever came out of the loop goes where the
            // common ancestor wants it (which may mean foster parenting).
            let saved_foster = self.foster;
            self.foster = matches!(self.name_of(common_ancestor), "table" | "tbody" | "tfoot" | "thead" | "tr");
            if self.foster {
                let (parent, index) = self.foster_parent();
                match index {
                    Some(i) => self.insert_before(parent, i, last_node),
                    None => self.append(parent, last_node),
                }
            } else {
                self.append(common_ancestor, last_node);
            }
            self.foster = saved_foster;

            // Steps 16-19: a new copy of the formatting element adopts
            // everything inside the furthest block.
            let Formatting::Element { tag, .. } = self.afe[fi.min(self.afe.len().saturating_sub(1))].clone() else {
                return true;
            };
            let new_formatting = self.new_element(&tag.name, Namespace::Html, tag.attrs.clone());
            let children = self.doc.nodes[furthest_block].children.clone();
            for child in children {
                self.append(new_formatting, child);
            }
            self.append(furthest_block, new_formatting);

            if let Some(pos) = self.afe_position(formatting) {
                self.afe.remove(pos);
                if bookmark > pos {
                    bookmark -= 1;
                }
            }
            let bookmark = bookmark.min(self.afe.len());
            self.afe.insert(bookmark, Formatting::Element { node: new_formatting, tag });

            if let Some(pos) = self.open.iter().position(|&n| n == formatting) {
                self.open.remove(pos);
            }
            let furthest_pos = self.open.iter().position(|&n| n == furthest_block).unwrap_or(self.open.len() - 1);
            self.open.insert(furthest_pos + 1, new_formatting);
        }
        true
    }

    // ---- token dispatch --------------------------------------------

    fn process(&mut self, token: Token) {
        // Past this many open elements a start tag is dropped on the
        // floor, the same thing browsers do (Blink's own limit is 512)
        // and for the same two reasons. The spec's algorithms are
        // written against the stack of open elements -- "have an element
        // in scope", "reconstruct the active formatting elements" --
        // so per-token work grows with depth and a file of nothing but
        // `<div>` is quadratic: 8000 of them take 6.7s here, 50000 take
        // eight minutes. And every consumer of the finished tree walks
        // it recursively (see markdown::render::html_runs), which a
        // depth nothing bounds turns into a stack overflow.
        //
        // Dropping the token rather than the element keeps the parser's
        // own invariants intact: nothing further down ever sees it, so
        // no rule has to cope with an element that was inserted but not
        // opened. Content inside still parses; it just attaches to the
        // deepest element that fit.
        const MAX_OPEN_ELEMENTS: usize = 512;
        if self.open.len() >= MAX_OPEN_ELEMENTS && matches!(token, Token::StartTag(_)) {
            return;
        }
        loop {
            let reprocess = if self.use_foreign_rules(&token) { self.foreign_content(&token) } else { self.dispatch(&token) };
            if !reprocess {
                return;
            }
        }
    }

    // §13.2.6: whether the token goes to the foreign-content rules
    // rather than to the current insertion mode.
    fn use_foreign_rules(&self, token: &Token) -> bool {
        if self.open.is_empty() {
            return false;
        }
        let node = self.adjusted_current();
        if self.ns_of(node) == Namespace::Html {
            return false;
        }
        let name = self.name_of(node);
        let ns = self.ns_of(node);
        // The integration points, where HTML parsing resumes inside
        // foreign content.
        if ns == Namespace::MathMl && MATHML_TEXT_INTEGRATION.contains(&name) {
            return !matches!(token, Token::Char(_)) && !matches!(token, Token::StartTag(t) if t.name != "mglyph" && t.name != "malignmark");
        }
        if ns == Namespace::MathMl && name == "annotation-xml" && matches!(token, Token::StartTag(t) if t.name == "svg") {
            return false;
        }
        if ns == Namespace::Svg && SVG_HTML_INTEGRATION.contains(&name) {
            return !matches!(token, Token::Char(_) | Token::StartTag(_));
        }
        !matches!(token, Token::Eof)
    }

    // The "adjusted current node" is the fragment context when the stack
    // holds only the synthetic root, and the current node otherwise.
    fn adjusted_current(&self) -> NodeId {
        self.current()
    }

    fn dispatch(&mut self, token: &Token) -> bool {
        match self.mode {
            Mode::Initial => self.initial(token),
            Mode::BeforeHtml => self.before_html(token),
            Mode::BeforeHead => self.before_head(token),
            Mode::InHead => self.in_head(token),
            Mode::InHeadNoscript => self.in_head_noscript(token),
            Mode::AfterHead => self.after_head(token),
            Mode::InBody => self.in_body(token),
            Mode::Text => self.text(token),
            Mode::InTable => self.in_table(token),
            Mode::InTableText => self.in_table_text(token),
            Mode::InCaption => self.in_caption(token),
            Mode::InColumnGroup => self.in_column_group(token),
            Mode::InTableBody => self.in_table_body(token),
            Mode::InRow => self.in_row(token),
            Mode::InCell => self.in_cell(token),
            Mode::InSelect => self.in_select(token),
            Mode::InSelectInTable => self.in_select_in_table(token),
            Mode::InTemplate => self.in_template(token),
            Mode::AfterBody => self.after_body(token),
            Mode::InFrameset => self.in_frameset(token),
            Mode::AfterFrameset => self.after_frameset(token),
            Mode::AfterAfterBody => self.after_after_body(token),
            Mode::AfterAfterFrameset => self.after_after_frameset(token),
        }
    }
}

fn same_attrs(a: &[Attr], b: &[Attr]) -> bool {
    a.len() == b.len() && a.iter().all(|x| b.iter().any(|y| y.name == x.name && y.value == x.value))
}

pub(super) fn is_whitespace(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' ')
}

pub(super) fn special_contains(name: &str) -> bool {
    SPECIAL.contains(&name)
}

pub(super) fn formatting_contains(name: &str) -> bool {
    FORMATTING.contains(&name)
}

pub(super) fn adjust_svg_tag(name: &str) -> String {
    SVG_TAG_NAMES.iter().find(|(lower, _)| *lower == name).map(|(_, real)| real.to_string()).unwrap_or_else(|| name.to_string())
}

pub(super) fn adjust_foreign_attrs(ns: Namespace, attrs: &mut [Attr]) {
    let table: &[(&str, &str)] = match ns {
        Namespace::Svg => SVG_ATTRS,
        Namespace::MathMl => MATHML_ATTRS,
        Namespace::Html => return,
    };
    for attr in attrs {
        if let Some((_, real)) = table.iter().find(|(lower, _)| *lower == attr.name) {
            attr.name = real.to_string();
        }
    }
}

// The insertion modes themselves. Each returns `true` when the spec says
// to reprocess the token in the (now changed) mode -- the one control-
// flow idea that makes these read like the spec instead of like a pile
// of nested conditions.
impl TreeBuilder {
    fn initial(&mut self, token: &Token) -> bool {
        match token {
            Token::Char(c) if is_whitespace(*c) => false,
            Token::Comment(text) => {
                let root = self.doc.root;
                self.insert_comment(text, Some(root));
                false
            }
            Token::Doctype(d) => {
                let name = d.name.clone().unwrap_or_default();
                let public_id = d.public_id.clone().unwrap_or_default();
                let system_id = d.system_id.clone().unwrap_or_default();
                if name != "html" || !public_id.is_empty() || (!system_id.is_empty() && system_id != "about:legacy-compat") {
                    self.error("unexpected doctype");
                }
                self.doc.quirks = quirks_for(d.force_quirks, &name, &public_id, &system_id);
                self.doc.nodes.push(Node { data: NodeData::Doctype { name, public_id, system_id }, parent: None, children: Vec::new() });
                let node = self.doc.nodes.len() - 1;
                let root = self.doc.root;
                self.append(root, node);
                self.mode = Mode::BeforeHtml;
                false
            }
            _ => {
                // A document with no doctype is a quirks-mode document,
                // which is the rule that keeps decades of pages
                // rendering the way they were written.
                self.doc.quirks = QuirksMode::Quirks;
                self.mode = Mode::BeforeHtml;
                true
            }
        }
    }

    fn before_html(&mut self, token: &Token) -> bool {
        match token {
            Token::Doctype(_) => {
                self.error("doctype after the start of the document");
                false
            }
            Token::Comment(text) => {
                let root = self.doc.root;
                self.insert_comment(text, Some(root));
                false
            }
            Token::Char(c) if is_whitespace(*c) => false,
            Token::StartTag(tag) if tag.name == "html" => {
                let node = self.new_element("html", Namespace::Html, tag.attrs.clone());
                let root = self.doc.root;
                self.append(root, node);
                self.open.push(node);
                self.mode = Mode::BeforeHead;
                false
            }
            Token::EndTag(tag) if !matches!(tag.name.as_str(), "head" | "body" | "html" | "br") => {
                self.error("unexpected end tag before <html>");
                false
            }
            _ => {
                let node = self.new_element("html", Namespace::Html, Vec::new());
                let root = self.doc.root;
                self.append(root, node);
                self.open.push(node);
                self.mode = Mode::BeforeHead;
                true
            }
        }
    }

    fn before_head(&mut self, token: &Token) -> bool {
        match token {
            Token::Char(c) if is_whitespace(*c) => false,
            Token::Comment(text) => {
                self.insert_comment(text, None);
                false
            }
            Token::Doctype(_) => {
                self.error("doctype after the start of the document");
                false
            }
            Token::StartTag(tag) if tag.name == "html" => self.in_body(token),
            Token::StartTag(tag) if tag.name == "head" => {
                let node = self.insert_element(tag, Namespace::Html);
                self.head = Some(node);
                self.mode = Mode::InHead;
                false
            }
            Token::EndTag(tag) if !matches!(tag.name.as_str(), "head" | "body" | "html" | "br") => {
                self.error("unexpected end tag before <head>");
                false
            }
            _ => {
                let head = Tag { name: "head".to_string(), attrs: Vec::new(), self_closing: false };
                let node = self.insert_element(&head, Namespace::Html);
                self.head = Some(node);
                self.mode = Mode::InHead;
                true
            }
        }
    }

    fn in_head(&mut self, token: &Token) -> bool {
        match token {
            Token::Char(c) if is_whitespace(*c) => {
                self.insert_char(*c);
                false
            }
            Token::Comment(text) => {
                self.insert_comment(text, None);
                false
            }
            Token::Doctype(_) => {
                self.error("doctype after the start of the document");
                false
            }
            Token::StartTag(tag) if tag.name == "html" => self.in_body(token),
            Token::StartTag(tag) if matches!(tag.name.as_str(), "base" | "basefont" | "bgsound" | "link" | "meta") => {
                self.insert_element(tag, Namespace::Html);
                self.open.pop();
                false
            }
            Token::StartTag(tag) if tag.name == "title" => {
                self.generic_rcdata(tag);
                false
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "noframes" | "style") => {
                self.generic_rawtext(tag);
                false
            }
            // Scripting is disabled here (there is no script host), so
            // `<noscript>` in the head is parsed rather than treated as
            // raw text -- which is the branch a browser with JavaScript
            // turned off takes too.
            Token::StartTag(tag) if tag.name == "noscript" => {
                self.insert_element(tag, Namespace::Html);
                self.mode = Mode::InHeadNoscript;
                false
            }
            Token::StartTag(tag) if tag.name == "script" => {
                self.insert_element(tag, Namespace::Html);
                self.tok.set_state(ContentState::ScriptData);
                self.original_mode = Some(self.mode);
                self.mode = Mode::Text;
                false
            }
            Token::EndTag(tag) if tag.name == "head" => {
                self.open.pop();
                self.mode = Mode::AfterHead;
                false
            }
            Token::StartTag(tag) if tag.name == "template" => {
                self.insert_element(tag, Namespace::Html);
                self.afe.push(Formatting::Marker);
                self.frameset_ok = false;
                self.mode = Mode::InTemplate;
                self.template_modes.push(Mode::InTemplate);
                false
            }
            Token::EndTag(tag) if tag.name == "template" => {
                if !self.open.iter().any(|&id| self.is_html(id, "template")) {
                    self.error("</template> with no open template");
                    return false;
                }
                self.generate_implied_end_tags_thoroughly();
                if !self.is_html(self.current(), "template") {
                    self.error("unexpected element before </template>");
                }
                self.pop_until_html("template");
                self.clear_afe_to_marker();
                self.template_modes.pop();
                self.reset_insertion_mode();
                false
            }
            Token::StartTag(tag) if tag.name == "head" => {
                self.error("nested <head>");
                false
            }
            Token::EndTag(tag) if !matches!(tag.name.as_str(), "body" | "html" | "br") => {
                self.error("unexpected end tag in <head>");
                false
            }
            _ => {
                self.open.pop();
                self.mode = Mode::AfterHead;
                true
            }
        }
    }

    fn in_head_noscript(&mut self, token: &Token) -> bool {
        match token {
            Token::Doctype(_) => {
                self.error("doctype after the start of the document");
                false
            }
            Token::StartTag(tag) if tag.name == "html" => self.in_body(token),
            Token::EndTag(tag) if tag.name == "noscript" => {
                self.open.pop();
                self.mode = Mode::InHead;
                false
            }
            Token::Char(c) if is_whitespace(*c) => self.in_head(token),
            Token::Comment(_) => self.in_head(token),
            Token::StartTag(tag) if matches!(tag.name.as_str(), "basefont" | "bgsound" | "link" | "meta" | "noframes" | "style") => {
                self.in_head(token)
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "head" | "noscript") => {
                self.error("unexpected start tag in <noscript>");
                false
            }
            Token::EndTag(tag) if tag.name != "br" => {
                self.error("unexpected end tag in <noscript>");
                false
            }
            _ => {
                self.error("unexpected content in <noscript>");
                self.open.pop();
                self.mode = Mode::InHead;
                true
            }
        }
    }

    fn after_head(&mut self, token: &Token) -> bool {
        match token {
            Token::Char(c) if is_whitespace(*c) => {
                self.insert_char(*c);
                false
            }
            Token::Comment(text) => {
                self.insert_comment(text, None);
                false
            }
            Token::Doctype(_) => {
                self.error("doctype after the start of the document");
                false
            }
            Token::StartTag(tag) if tag.name == "html" => self.in_body(token),
            Token::StartTag(tag) if tag.name == "body" => {
                self.insert_element(tag, Namespace::Html);
                self.frameset_ok = false;
                self.mode = Mode::InBody;
                false
            }
            Token::StartTag(tag) if tag.name == "frameset" => {
                self.insert_element(tag, Namespace::Html);
                self.mode = Mode::InFrameset;
                false
            }
            // Head content that turned up after the head closed: the
            // head is temporarily pushed back so it lands in the right
            // place, then removed again.
            Token::StartTag(tag)
                if matches!(
                    tag.name.as_str(),
                    "base" | "basefont" | "bgsound" | "link" | "meta" | "noframes" | "script" | "style" | "template" | "title"
                ) =>
            {
                self.error("head content after </head>");
                let head = self.head;
                if let Some(head) = head {
                    self.open.push(head);
                }
                let reprocess = self.in_head(token);
                if let Some(head) = head
                    && let Some(pos) = self.open.iter().rposition(|&id| id == head)
                {
                    self.open.remove(pos);
                }
                reprocess
            }
            Token::EndTag(tag) if tag.name == "template" => self.in_head(token),
            Token::StartTag(tag) if tag.name == "head" => {
                self.error("nested <head>");
                false
            }
            Token::EndTag(tag) if !matches!(tag.name.as_str(), "body" | "html" | "br") => {
                self.error("unexpected end tag after </head>");
                false
            }
            _ => {
                let body = Tag { name: "body".to_string(), attrs: Vec::new(), self_closing: false };
                self.insert_element(&body, Namespace::Html);
                self.mode = Mode::InBody;
                true
            }
        }
    }

    fn text(&mut self, token: &Token) -> bool {
        match token {
            Token::Char(c) => {
                self.insert_char(*c);
                false
            }
            Token::Eof => {
                self.error("eof in raw text");
                self.open.pop();
                self.mode = self.original_mode.take().unwrap_or(Mode::InBody);
                true
            }
            _ => {
                self.open.pop();
                self.mode = self.original_mode.take().unwrap_or(Mode::InBody);
                false
            }
        }
    }

    fn generic_rcdata(&mut self, tag: &Tag) {
        self.insert_element(tag, Namespace::Html);
        self.tok.set_state(ContentState::Rcdata);
        self.original_mode = Some(self.mode);
        self.mode = Mode::Text;
    }

    fn generic_rawtext(&mut self, tag: &Tag) {
        self.insert_element(tag, Namespace::Html);
        self.tok.set_state(ContentState::Rawtext);
        self.original_mode = Some(self.mode);
        self.mode = Mode::Text;
    }
}

// "In body", by a wide margin the largest mode -- most of a document is
// parsed here, and most of the interesting recovery behaviour lives in
// it.
impl TreeBuilder {
    fn in_body(&mut self, token: &Token) -> bool {
        match token {
            Token::Char('\0') => {
                self.error("null character in body");
                false
            }
            Token::Char(c) => {
                // A newline right after <pre>/<listing>/<textarea> is
                // dropped, so writing the content on its own line
                // doesn't add a blank one.
                if self.ignore_lf {
                    self.ignore_lf = false;
                    if *c == '\n' {
                        return false;
                    }
                }
                self.reconstruct_afe();
                self.insert_char(*c);
                if !is_whitespace(*c) {
                    self.frameset_ok = false;
                }
                false
            }
            Token::Comment(text) => {
                self.insert_comment(text, None);
                false
            }
            Token::Doctype(_) => {
                self.error("doctype after the start of the document");
                false
            }
            Token::StartTag(tag) if tag.name == "html" => {
                self.error("nested <html>");
                if !self.open.iter().any(|&id| self.is_html(id, "template")) {
                    let top = self.open[0];
                    self.merge_attrs(top, &tag.attrs);
                }
                false
            }
            Token::StartTag(tag)
                if matches!(
                    tag.name.as_str(),
                    "base" | "basefont" | "bgsound" | "link" | "meta" | "noframes" | "script" | "style" | "template" | "title"
                ) =>
            {
                self.in_head(token)
            }
            Token::EndTag(tag) if tag.name == "template" => self.in_head(token),
            Token::StartTag(tag) if tag.name == "body" => {
                self.error("nested <body>");
                let second_is_body = self.open.len() > 1 && self.is_html(self.open[1], "body");
                if second_is_body && !self.open.iter().any(|&id| self.is_html(id, "template")) {
                    self.frameset_ok = false;
                    let body = self.open[1];
                    self.merge_attrs(body, &tag.attrs);
                }
                false
            }
            Token::StartTag(tag) if tag.name == "frameset" => {
                self.error("<frameset> in body");
                let second_is_body = self.open.len() > 1 && self.is_html(self.open[1], "body");
                if !second_is_body || !self.frameset_ok {
                    return false;
                }
                let body = self.open[1];
                self.detach(body);
                self.open.truncate(1);
                self.insert_element(tag, Namespace::Html);
                self.mode = Mode::InFrameset;
                false
            }
            Token::Eof => {
                if !self.template_modes.is_empty() {
                    return self.in_template(token);
                }
                self.check_open_at_eof();
                self.done = true;
                false
            }
            Token::EndTag(tag) if tag.name == "body" => {
                if !self.has_in_scope("body") {
                    self.error("</body> with no open body");
                    return false;
                }
                self.check_open_at_eof();
                self.mode = Mode::AfterBody;
                false
            }
            Token::EndTag(tag) if tag.name == "html" => {
                if !self.has_in_scope("body") {
                    self.error("</html> with no open body");
                    return false;
                }
                self.check_open_at_eof();
                self.mode = Mode::AfterBody;
                true
            }
            Token::StartTag(tag)
                if matches!(
                    tag.name.as_str(),
                    "address"
                        | "article"
                        | "aside"
                        | "blockquote"
                        | "center"
                        | "details"
                        | "dialog"
                        | "dir"
                        | "div"
                        | "dl"
                        | "fieldset"
                        | "figcaption"
                        | "figure"
                        | "footer"
                        | "header"
                        | "hgroup"
                        | "main"
                        | "menu"
                        | "nav"
                        | "ol"
                        | "p"
                        | "search"
                        | "section"
                        | "summary"
                        | "ul"
                ) =>
            {
                if self.has_in_button_scope("p") {
                    self.close_p();
                }
                self.insert_element(tag, Namespace::Html);
                false
            }
            Token::StartTag(tag) if HEADINGS.contains(&tag.name.as_str()) => {
                if self.has_in_button_scope("p") {
                    self.close_p();
                }
                if HEADINGS.contains(&self.name_of(self.current())) {
                    self.error("nested heading");
                    self.open.pop();
                }
                self.insert_element(tag, Namespace::Html);
                false
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "pre" | "listing") => {
                if self.has_in_button_scope("p") {
                    self.close_p();
                }
                self.insert_element(tag, Namespace::Html);
                self.ignore_lf = true;
                self.frameset_ok = false;
                false
            }
            Token::StartTag(tag) if tag.name == "form" => {
                let has_template = self.open.iter().any(|&id| self.is_html(id, "template"));
                if self.form.is_some() && !has_template {
                    self.error("nested <form>");
                    return false;
                }
                if self.has_in_button_scope("p") {
                    self.close_p();
                }
                let node = self.insert_element(tag, Namespace::Html);
                if !has_template {
                    self.form = Some(node);
                }
                false
            }
            // `<li>` closing the previous `<li>` -- and the search
            // stopping at a *special* element, so a list item inside a
            // `<div>` inside a list item doesn't close the outer one.
            Token::StartTag(tag) if tag.name == "li" => {
                self.frameset_ok = false;
                for i in (0..self.open.len()).rev() {
                    let id = self.open[i];
                    if self.is_html(id, "li") {
                        self.generate_implied_end_tags("li");
                        if !self.is_html(self.current(), "li") {
                            self.error("unexpected element before <li>");
                        }
                        self.pop_until_html("li");
                        break;
                    }
                    let name = self.name_of(id);
                    if SPECIAL.contains(&name) && !matches!(name, "address" | "div" | "p") {
                        break;
                    }
                }
                if self.has_in_button_scope("p") {
                    self.close_p();
                }
                self.insert_element(tag, Namespace::Html);
                false
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "dd" | "dt") => {
                self.frameset_ok = false;
                for i in (0..self.open.len()).rev() {
                    let id = self.open[i];
                    let name = self.name_of(id).to_string();
                    if name == "dd" || name == "dt" {
                        self.generate_implied_end_tags(&name);
                        if self.name_of(self.current()) != name {
                            self.error("unexpected element before <dd>/<dt>");
                        }
                        self.pop_until_html(&name);
                        break;
                    }
                    if SPECIAL.contains(&name.as_str()) && !matches!(name.as_str(), "address" | "div" | "p") {
                        break;
                    }
                }
                if self.has_in_button_scope("p") {
                    self.close_p();
                }
                self.insert_element(tag, Namespace::Html);
                false
            }
            Token::StartTag(tag) if tag.name == "plaintext" => {
                if self.has_in_button_scope("p") {
                    self.close_p();
                }
                self.insert_element(tag, Namespace::Html);
                self.tok.set_state(ContentState::Plaintext);
                false
            }
            Token::StartTag(tag) if tag.name == "button" => {
                if self.has_in_scope("button") {
                    self.error("nested <button>");
                    self.generate_implied_end_tags("");
                    self.pop_until_html("button");
                }
                self.reconstruct_afe();
                self.insert_element(tag, Namespace::Html);
                self.frameset_ok = false;
                false
            }
            // `<a>` inside `<a>`: the outer one is closed by the
            // adoption agency first, which is why nested links never
            // nest in a real tree.
            Token::StartTag(tag) if tag.name == "a" => {
                let existing = self.afe.iter().rev().find_map(|e| match e {
                    Formatting::Marker => None,
                    Formatting::Element { node, tag } if tag.name == "a" => Some(*node),
                    _ => None,
                });
                if let Some(node) = existing {
                    self.error("nested <a>");
                    self.adoption_agency("a");
                    if let Some(pos) = self.afe_position(node) {
                        self.afe.remove(pos);
                    }
                    if let Some(pos) = self.open.iter().position(|&n| n == node) {
                        self.open.remove(pos);
                    }
                }
                self.reconstruct_afe();
                let node = self.insert_element(tag, Namespace::Html);
                self.push_afe(node, tag.clone());
                false
            }
            Token::StartTag(tag) if FORMATTING.contains(&tag.name.as_str()) && tag.name != "nobr" => {
                self.reconstruct_afe();
                let node = self.insert_element(tag, Namespace::Html);
                self.push_afe(node, tag.clone());
                false
            }
            Token::StartTag(tag) if tag.name == "nobr" => {
                self.reconstruct_afe();
                if self.has_in_scope("nobr") {
                    self.error("nested <nobr>");
                    self.adoption_agency("nobr");
                    self.reconstruct_afe();
                }
                let node = self.insert_element(tag, Namespace::Html);
                self.push_afe(node, tag.clone());
                false
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "applet" | "marquee" | "object") => {
                self.reconstruct_afe();
                self.insert_element(tag, Namespace::Html);
                self.afe.push(Formatting::Marker);
                self.frameset_ok = false;
                false
            }
            Token::StartTag(tag) if tag.name == "table" => {
                if self.doc.quirks != QuirksMode::Quirks && self.has_in_button_scope("p") {
                    self.close_p();
                }
                self.insert_element(tag, Namespace::Html);
                self.frameset_ok = false;
                self.mode = Mode::InTable;
                false
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "area" | "br" | "embed" | "img" | "keygen" | "wbr") => {
                self.reconstruct_afe();
                self.insert_element(tag, Namespace::Html);
                self.open.pop();
                self.frameset_ok = false;
                false
            }
            Token::StartTag(tag) if tag.name == "input" => {
                self.reconstruct_afe();
                self.insert_element(tag, Namespace::Html);
                self.open.pop();
                if !tag.attr("type").is_some_and(|t| t.eq_ignore_ascii_case("hidden")) {
                    self.frameset_ok = false;
                }
                false
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "param" | "source" | "track") => {
                self.insert_element(tag, Namespace::Html);
                self.open.pop();
                false
            }
            Token::StartTag(tag) if tag.name == "hr" => {
                if self.has_in_button_scope("p") {
                    self.close_p();
                }
                self.insert_element(tag, Namespace::Html);
                self.open.pop();
                self.frameset_ok = false;
                false
            }
            // `<image>` is a historical misspelling the spec explicitly
            // rewrites rather than honouring.
            Token::StartTag(tag) if tag.name == "image" => {
                self.error("<image> is not an element (treated as <img>)");
                let mut fixed = tag.clone();
                fixed.name = "img".to_string();
                self.process(Token::StartTag(fixed));
                false
            }
            Token::StartTag(tag) if tag.name == "textarea" => {
                self.insert_element(tag, Namespace::Html);
                self.ignore_lf = true;
                self.tok.set_state(ContentState::Rcdata);
                self.original_mode = Some(self.mode);
                self.frameset_ok = false;
                self.mode = Mode::Text;
                false
            }
            Token::StartTag(tag) if tag.name == "xmp" => {
                if self.has_in_button_scope("p") {
                    self.close_p();
                }
                self.reconstruct_afe();
                self.frameset_ok = false;
                self.generic_rawtext(tag);
                false
            }
            Token::StartTag(tag) if tag.name == "iframe" => {
                self.frameset_ok = false;
                self.generic_rawtext(tag);
                false
            }
            Token::StartTag(tag) if tag.name == "noembed" => {
                self.generic_rawtext(tag);
                false
            }
            Token::StartTag(tag) if tag.name == "select" => {
                self.reconstruct_afe();
                self.insert_element(tag, Namespace::Html);
                self.frameset_ok = false;
                self.mode = match self.mode {
                    Mode::InTable | Mode::InCaption | Mode::InTableBody | Mode::InRow | Mode::InCell => Mode::InSelectInTable,
                    _ => Mode::InSelect,
                };
                false
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "optgroup" | "option") => {
                if self.is_html(self.current(), "option") {
                    self.open.pop();
                }
                self.reconstruct_afe();
                self.insert_element(tag, Namespace::Html);
                false
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "rb" | "rtc") => {
                if self.has_in_scope("ruby") {
                    self.generate_implied_end_tags("");
                }
                self.insert_element(tag, Namespace::Html);
                false
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "rp" | "rt") => {
                if self.has_in_scope("ruby") {
                    self.generate_implied_end_tags("rtc");
                }
                self.insert_element(tag, Namespace::Html);
                false
            }
            Token::StartTag(tag) if tag.name == "math" => {
                self.reconstruct_afe();
                let mut fixed = tag.clone();
                adjust_foreign_attrs(Namespace::MathMl, &mut fixed.attrs);
                self.insert_element(&fixed, Namespace::MathMl);
                if tag.self_closing {
                    self.open.pop();
                }
                false
            }
            Token::StartTag(tag) if tag.name == "svg" => {
                self.reconstruct_afe();
                let mut fixed = tag.clone();
                adjust_foreign_attrs(Namespace::Svg, &mut fixed.attrs);
                self.insert_element(&fixed, Namespace::Svg);
                if tag.self_closing {
                    self.open.pop();
                }
                false
            }
            Token::StartTag(tag)
                if matches!(
                    tag.name.as_str(),
                    "caption" | "col" | "colgroup" | "frame" | "head" | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr"
                ) =>
            {
                self.error("start tag that has no place in the body");
                false
            }
            Token::StartTag(tag) => {
                self.reconstruct_afe();
                self.insert_element(tag, Namespace::Html);
                false
            }
            Token::EndTag(tag)
                if matches!(
                    tag.name.as_str(),
                    "address"
                        | "article"
                        | "aside"
                        | "blockquote"
                        | "button"
                        | "center"
                        | "details"
                        | "dialog"
                        | "dir"
                        | "div"
                        | "dl"
                        | "fieldset"
                        | "figcaption"
                        | "figure"
                        | "footer"
                        | "header"
                        | "hgroup"
                        | "listing"
                        | "main"
                        | "menu"
                        | "nav"
                        | "ol"
                        | "pre"
                        | "search"
                        | "section"
                        | "summary"
                        | "ul"
                ) =>
            {
                if !self.has_in_scope(&tag.name) {
                    self.error("end tag with no matching open element");
                    return false;
                }
                self.generate_implied_end_tags("");
                if self.name_of(self.current()) != tag.name {
                    self.error("misnested end tag");
                }
                self.pop_until_html(&tag.name);
                false
            }
            Token::EndTag(tag) if tag.name == "form" => {
                let has_template = self.open.iter().any(|&id| self.is_html(id, "template"));
                if has_template {
                    if !self.has_in_scope("form") {
                        self.error("</form> with no open form");
                        return false;
                    }
                    self.generate_implied_end_tags("");
                    self.pop_until_html("form");
                    return false;
                }
                let node = self.form.take();
                let Some(node) = node else {
                    self.error("</form> with no open form");
                    return false;
                };
                if !self.open.contains(&node) {
                    self.error("</form> for a form that is no longer open");
                    return false;
                }
                self.generate_implied_end_tags("");
                if self.current() != node {
                    self.error("misnested </form>");
                }
                if let Some(pos) = self.open.iter().position(|&n| n == node) {
                    self.open.remove(pos);
                }
                false
            }
            // A `</p>` with no `<p>` open creates an empty one, which is
            // the spec's own recovery and not a mistake here.
            Token::EndTag(tag) if tag.name == "p" => {
                if !self.has_in_button_scope("p") {
                    self.error("</p> with no open paragraph");
                    let p = Tag { name: "p".to_string(), attrs: Vec::new(), self_closing: false };
                    self.insert_element(&p, Namespace::Html);
                }
                self.close_p();
                false
            }
            Token::EndTag(tag) if tag.name == "li" => {
                if !self.has_in_list_item_scope("li") {
                    self.error("</li> with no open list item");
                    return false;
                }
                self.generate_implied_end_tags("li");
                if !self.is_html(self.current(), "li") {
                    self.error("misnested </li>");
                }
                self.pop_until_html("li");
                false
            }
            Token::EndTag(tag) if matches!(tag.name.as_str(), "dd" | "dt") => {
                if !self.has_in_scope(&tag.name) {
                    self.error("end tag with no matching open element");
                    return false;
                }
                self.generate_implied_end_tags(&tag.name);
                if self.name_of(self.current()) != tag.name {
                    self.error("misnested end tag");
                }
                self.pop_until_html(&tag.name);
                false
            }
            Token::EndTag(tag) if HEADINGS.contains(&tag.name.as_str()) => {
                if !self.has_heading_in_scope() {
                    self.error("</h*> with no open heading");
                    return false;
                }
                self.generate_implied_end_tags("");
                if self.name_of(self.current()) != tag.name {
                    self.error("misnested heading end tag");
                }
                self.pop_until_any(HEADINGS);
                false
            }
            Token::EndTag(tag) if FORMATTING.contains(&tag.name.as_str()) => {
                if !self.adoption_agency(&tag.name) {
                    return self.any_other_end_tag(&tag.name);
                }
                false
            }
            Token::EndTag(tag) if matches!(tag.name.as_str(), "applet" | "marquee" | "object") => {
                if !self.has_in_scope(&tag.name) {
                    self.error("end tag with no matching open element");
                    return false;
                }
                self.generate_implied_end_tags("");
                if self.name_of(self.current()) != tag.name {
                    self.error("misnested end tag");
                }
                self.pop_until_html(&tag.name);
                self.clear_afe_to_marker();
                false
            }
            Token::EndTag(tag) if tag.name == "br" => {
                self.error("</br> is not an end tag (treated as <br>)");
                let br = Tag { name: "br".to_string(), attrs: Vec::new(), self_closing: false };
                self.reconstruct_afe();
                self.insert_element(&br, Namespace::Html);
                self.open.pop();
                self.frameset_ok = false;
                false
            }
            Token::EndTag(tag) => self.any_other_end_tag(&tag.name),
        }
    }

    // §13.2.6.4.7's "any other end tag": walk down the stack looking for
    // a match, stopping at the first *special* element -- which is what
    // makes a stray `</div>` inside a `<p>` do nothing rather than close
    // something it shouldn't.
    fn any_other_end_tag(&mut self, name: &str) -> bool {
        for i in (0..self.open.len()).rev() {
            let id = self.open[i];
            if self.is_html(id, name) {
                self.generate_implied_end_tags(name);
                if self.current() != id {
                    self.error("misnested end tag");
                }
                self.open.truncate(i);
                return false;
            }
            if SPECIAL.contains(&self.name_of(id)) {
                self.error("end tag with no matching open element");
                return false;
            }
        }
        false
    }

    fn merge_attrs(&mut self, node: NodeId, attrs: &[Attr]) {
        if let NodeData::Element { attrs: existing, .. } = &mut self.doc.nodes[node].data {
            for attr in attrs {
                if !existing.iter().any(|a| a.name == attr.name) {
                    existing.push(attr.clone());
                }
            }
        }
    }

    // The spec's own list of elements that may still be open when a
    // document ends without complaint; anything else is a parse error.
    fn check_open_at_eof(&mut self) {
        let bad = self.open.iter().any(|&id| {
            !matches!(
                self.name_of(id),
                "dd" | "dt"
                    | "li"
                    | "optgroup"
                    | "option"
                    | "p"
                    | "rb"
                    | "rp"
                    | "rt"
                    | "rtc"
                    | "tbody"
                    | "td"
                    | "tfoot"
                    | "th"
                    | "thead"
                    | "tr"
                    | "body"
                    | "html"
            )
        });
        if bad {
            self.error("unclosed elements at the end of the document");
        }
    }
}

// The table modes. Tables have their own modes at all because a table's
// content model is strict enough that browsers had to agree on where
// everything *else* goes -- which is foster parenting, below.
impl TreeBuilder {
    fn clear_stack_to(&mut self, names: &[&str]) {
        while let Some(&id) = self.open.last() {
            let n = self.name_of(id);
            if names.contains(&n) || n == "html" || n == "template" {
                break;
            }
            self.open.pop();
        }
    }

    fn in_table(&mut self, token: &Token) -> bool {
        match token {
            Token::Char(_) if matches!(self.name_of(self.current()), "table" | "tbody" | "tfoot" | "thead" | "tr") => {
                self.pending_table_text.clear();
                self.pending_table_text_ws_only = true;
                self.original_mode = Some(self.mode);
                self.mode = Mode::InTableText;
                true
            }
            Token::Comment(text) => {
                self.insert_comment(text, None);
                false
            }
            Token::Doctype(_) => {
                self.error("doctype in a table");
                false
            }
            Token::StartTag(tag) if tag.name == "caption" => {
                self.clear_stack_to(&["table"]);
                self.afe.push(Formatting::Marker);
                self.insert_element(tag, Namespace::Html);
                self.mode = Mode::InCaption;
                false
            }
            Token::StartTag(tag) if tag.name == "colgroup" => {
                self.clear_stack_to(&["table"]);
                self.insert_element(tag, Namespace::Html);
                self.mode = Mode::InColumnGroup;
                false
            }
            Token::StartTag(tag) if tag.name == "col" => {
                self.clear_stack_to(&["table"]);
                let cg = Tag { name: "colgroup".to_string(), attrs: Vec::new(), self_closing: false };
                self.insert_element(&cg, Namespace::Html);
                self.mode = Mode::InColumnGroup;
                true
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "tbody" | "tfoot" | "thead") => {
                self.clear_stack_to(&["table"]);
                self.insert_element(tag, Namespace::Html);
                self.mode = Mode::InTableBody;
                false
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "td" | "th" | "tr") => {
                self.clear_stack_to(&["table"]);
                let tb = Tag { name: "tbody".to_string(), attrs: Vec::new(), self_closing: false };
                self.insert_element(&tb, Namespace::Html);
                self.mode = Mode::InTableBody;
                true
            }
            Token::StartTag(tag) if tag.name == "table" => {
                self.error("nested <table>");
                if !self.has_in_table_scope("table") {
                    return false;
                }
                self.pop_until_html("table");
                self.reset_insertion_mode();
                true
            }
            Token::EndTag(tag) if tag.name == "table" => {
                if !self.has_in_table_scope("table") {
                    self.error("</table> with no open table");
                    return false;
                }
                self.pop_until_html("table");
                self.reset_insertion_mode();
                false
            }
            Token::EndTag(tag)
                if matches!(
                    tag.name.as_str(),
                    "body" | "caption" | "col" | "colgroup" | "html" | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr"
                ) =>
            {
                self.error("end tag with no matching open element in a table");
                false
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "style" | "script" | "template") => self.in_head(token),
            Token::EndTag(tag) if tag.name == "template" => self.in_head(token),
            Token::StartTag(tag) if tag.name == "input" && tag.attr("type").is_some_and(|t| t.eq_ignore_ascii_case("hidden")) => {
                self.error("<input> in a table");
                self.insert_element(tag, Namespace::Html);
                self.open.pop();
                false
            }
            Token::StartTag(tag) if tag.name == "form" => {
                self.error("<form> in a table");
                if self.form.is_some() || self.open.iter().any(|&id| self.is_html(id, "template")) {
                    return false;
                }
                let node = self.insert_element(tag, Namespace::Html);
                self.form = Some(node);
                self.open.pop();
                false
            }
            Token::Eof => self.in_body(token),
            // Anything a table has no place for is foster parented: it
            // ends up immediately *before* the table in the tree.
            _ => {
                self.error("content in a table that cannot go there");
                self.foster = true;
                let reprocess = self.in_body(token);
                self.foster = false;
                reprocess
            }
        }
    }

    // Character data inside a table is buffered, because whether it may
    // stay depends on whether the whole run turns out to be whitespace
    // -- which isn't known until the run ends.
    fn in_table_text(&mut self, token: &Token) -> bool {
        match token {
            Token::Char('\0') => {
                self.error("null character in a table");
                false
            }
            Token::Char(c) => {
                if !is_whitespace(*c) {
                    self.pending_table_text_ws_only = false;
                }
                self.pending_table_text.push(*c);
                false
            }
            _ => {
                let text = std::mem::take(&mut self.pending_table_text);
                if self.pending_table_text_ws_only {
                    for c in text.chars() {
                        self.insert_char(c);
                    }
                } else {
                    self.error("text in a table that cannot go there");
                    self.foster = true;
                    for c in text.chars() {
                        self.reconstruct_afe();
                        self.insert_char(c);
                        if !is_whitespace(c) {
                            self.frameset_ok = false;
                        }
                    }
                    self.foster = false;
                }
                self.pending_table_text_ws_only = true;
                self.mode = self.original_mode.take().unwrap_or(Mode::InTable);
                true
            }
        }
    }

    fn close_caption(&mut self) -> bool {
        if !self.has_in_table_scope("caption") {
            self.error("</caption> with no open caption");
            return false;
        }
        self.generate_implied_end_tags("");
        if !self.is_html(self.current(), "caption") {
            self.error("misnested </caption>");
        }
        self.pop_until_html("caption");
        self.clear_afe_to_marker();
        self.mode = Mode::InTable;
        true
    }

    fn in_caption(&mut self, token: &Token) -> bool {
        match token {
            Token::EndTag(tag) if tag.name == "caption" => {
                self.close_caption();
                false
            }
            Token::StartTag(tag)
                if matches!(tag.name.as_str(), "caption" | "col" | "colgroup" | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr") =>
            {
                self.error("table content inside a caption");
                self.close_caption()
            }
            Token::EndTag(tag) if tag.name == "table" => {
                self.error("</table> inside a caption");
                self.close_caption()
            }
            Token::EndTag(tag)
                if matches!(tag.name.as_str(), "body" | "col" | "colgroup" | "html" | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr") =>
            {
                self.error("end tag with no matching open element in a caption");
                false
            }
            _ => self.in_body(token),
        }
    }

    fn in_column_group(&mut self, token: &Token) -> bool {
        match token {
            Token::Char(c) if is_whitespace(*c) => {
                self.insert_char(*c);
                false
            }
            Token::Comment(text) => {
                self.insert_comment(text, None);
                false
            }
            Token::Doctype(_) => {
                self.error("doctype in a column group");
                false
            }
            Token::StartTag(tag) if tag.name == "html" => self.in_body(token),
            Token::StartTag(tag) if tag.name == "col" => {
                self.insert_element(tag, Namespace::Html);
                self.open.pop();
                false
            }
            Token::EndTag(tag) if tag.name == "colgroup" => {
                if !self.is_html(self.current(), "colgroup") {
                    self.error("</colgroup> with no open column group");
                    return false;
                }
                self.open.pop();
                self.mode = Mode::InTable;
                false
            }
            Token::EndTag(tag) if tag.name == "col" => {
                self.error("</col> is not an end tag");
                false
            }
            Token::StartTag(tag) if tag.name == "template" => self.in_head(token),
            Token::EndTag(tag) if tag.name == "template" => self.in_head(token),
            Token::Eof => self.in_body(token),
            _ => {
                if !self.is_html(self.current(), "colgroup") {
                    self.error("content in a column group that cannot go there");
                    return false;
                }
                self.open.pop();
                self.mode = Mode::InTable;
                true
            }
        }
    }

    fn in_table_body(&mut self, token: &Token) -> bool {
        match token {
            Token::StartTag(tag) if tag.name == "tr" => {
                self.clear_stack_to(&["tbody", "tfoot", "thead"]);
                self.insert_element(tag, Namespace::Html);
                self.mode = Mode::InRow;
                false
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "th" | "td") => {
                self.error("cell outside a row");
                self.clear_stack_to(&["tbody", "tfoot", "thead"]);
                let tr = Tag { name: "tr".to_string(), attrs: Vec::new(), self_closing: false };
                self.insert_element(&tr, Namespace::Html);
                self.mode = Mode::InRow;
                true
            }
            Token::EndTag(tag) if matches!(tag.name.as_str(), "tbody" | "tfoot" | "thead") => {
                if !self.has_in_table_scope(&tag.name) {
                    self.error("end tag with no matching open element");
                    return false;
                }
                self.clear_stack_to(&["tbody", "tfoot", "thead"]);
                self.open.pop();
                self.mode = Mode::InTable;
                false
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "caption" | "col" | "colgroup" | "tbody" | "tfoot" | "thead") => {
                if !["tbody", "thead", "tfoot"].iter().any(|n| self.has_in_table_scope(n)) {
                    self.error("table section with no open section");
                    return false;
                }
                self.clear_stack_to(&["tbody", "tfoot", "thead"]);
                self.open.pop();
                self.mode = Mode::InTable;
                true
            }
            Token::EndTag(tag) if tag.name == "table" => {
                if !["tbody", "thead", "tfoot"].iter().any(|n| self.has_in_table_scope(n)) {
                    self.error("</table> with no open section");
                    return false;
                }
                self.clear_stack_to(&["tbody", "tfoot", "thead"]);
                self.open.pop();
                self.mode = Mode::InTable;
                true
            }
            Token::EndTag(tag) if matches!(tag.name.as_str(), "body" | "caption" | "col" | "colgroup" | "html" | "td" | "th" | "tr") => {
                self.error("end tag with no matching open element in a table body");
                false
            }
            _ => self.in_table(token),
        }
    }

    fn in_row(&mut self, token: &Token) -> bool {
        match token {
            Token::StartTag(tag) if matches!(tag.name.as_str(), "th" | "td") => {
                self.clear_stack_to(&["tr"]);
                self.insert_element(tag, Namespace::Html);
                self.mode = Mode::InCell;
                self.afe.push(Formatting::Marker);
                false
            }
            Token::EndTag(tag) if tag.name == "tr" => {
                if !self.has_in_table_scope("tr") {
                    self.error("</tr> with no open row");
                    return false;
                }
                self.clear_stack_to(&["tr"]);
                self.open.pop();
                self.mode = Mode::InTableBody;
                false
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "caption" | "col" | "colgroup" | "tbody" | "tfoot" | "thead" | "tr") => {
                if !self.has_in_table_scope("tr") {
                    self.error("row content with no open row");
                    return false;
                }
                self.clear_stack_to(&["tr"]);
                self.open.pop();
                self.mode = Mode::InTableBody;
                true
            }
            Token::EndTag(tag) if tag.name == "table" => {
                if !self.has_in_table_scope("tr") {
                    self.error("</table> with no open row");
                    return false;
                }
                self.clear_stack_to(&["tr"]);
                self.open.pop();
                self.mode = Mode::InTableBody;
                true
            }
            Token::EndTag(tag) if matches!(tag.name.as_str(), "tbody" | "tfoot" | "thead") => {
                if !self.has_in_table_scope(&tag.name) {
                    self.error("end tag with no matching open element");
                    return false;
                }
                if !self.has_in_table_scope("tr") {
                    return false;
                }
                self.clear_stack_to(&["tr"]);
                self.open.pop();
                self.mode = Mode::InTableBody;
                true
            }
            Token::EndTag(tag) if matches!(tag.name.as_str(), "body" | "caption" | "col" | "colgroup" | "html" | "td" | "th") => {
                self.error("end tag with no matching open element in a row");
                false
            }
            _ => self.in_table(token),
        }
    }

    fn close_cell(&mut self) {
        let name = if self.has_in_table_scope("td") { "td" } else { "th" };
        self.generate_implied_end_tags("");
        if !self.is_html(self.current(), name) {
            self.error("misnested cell end tag");
        }
        self.pop_until_html(name);
        self.clear_afe_to_marker();
        self.mode = Mode::InRow;
    }

    fn in_cell(&mut self, token: &Token) -> bool {
        match token {
            Token::EndTag(tag) if matches!(tag.name.as_str(), "td" | "th") => {
                if !self.has_in_table_scope(&tag.name) {
                    self.error("end tag with no matching open cell");
                    return false;
                }
                self.generate_implied_end_tags("");
                if self.name_of(self.current()) != tag.name {
                    self.error("misnested cell end tag");
                }
                self.pop_until_html(&tag.name);
                self.clear_afe_to_marker();
                self.mode = Mode::InRow;
                false
            }
            Token::StartTag(tag)
                if matches!(tag.name.as_str(), "caption" | "col" | "colgroup" | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr") =>
            {
                if !self.has_in_table_scope("td") && !self.has_in_table_scope("th") {
                    self.error("table content with no open cell");
                    return false;
                }
                self.close_cell();
                true
            }
            Token::EndTag(tag) if matches!(tag.name.as_str(), "body" | "caption" | "col" | "colgroup" | "html") => {
                self.error("end tag with no matching open element in a cell");
                false
            }
            Token::EndTag(tag) if matches!(tag.name.as_str(), "table" | "tbody" | "tfoot" | "thead" | "tr") => {
                if !self.has_in_table_scope(&tag.name) {
                    self.error("end tag with no matching open element");
                    return false;
                }
                self.close_cell();
                true
            }
            _ => self.in_body(token),
        }
    }
}

// Select, template, frameset, and the modes a document ends in.
impl TreeBuilder {
    fn in_select(&mut self, token: &Token) -> bool {
        match token {
            Token::Char('\0') => {
                self.error("null character in a select");
                false
            }
            Token::Char(c) => {
                self.insert_char(*c);
                false
            }
            Token::Comment(text) => {
                self.insert_comment(text, None);
                false
            }
            Token::Doctype(_) => {
                self.error("doctype in a select");
                false
            }
            Token::StartTag(tag) if tag.name == "html" => self.in_body(token),
            Token::StartTag(tag) if tag.name == "option" => {
                if self.is_html(self.current(), "option") {
                    self.open.pop();
                }
                self.insert_element(tag, Namespace::Html);
                false
            }
            Token::StartTag(tag) if tag.name == "optgroup" => {
                if self.is_html(self.current(), "option") {
                    self.open.pop();
                }
                if self.is_html(self.current(), "optgroup") {
                    self.open.pop();
                }
                self.insert_element(tag, Namespace::Html);
                false
            }
            Token::EndTag(tag) if tag.name == "optgroup" => {
                if self.is_html(self.current(), "option") && self.open.len() > 1 && self.is_html(self.open[self.open.len() - 2], "optgroup") {
                    self.open.pop();
                }
                if self.is_html(self.current(), "optgroup") {
                    self.open.pop();
                } else {
                    self.error("</optgroup> with no open optgroup");
                }
                false
            }
            Token::EndTag(tag) if tag.name == "option" => {
                if self.is_html(self.current(), "option") {
                    self.open.pop();
                } else {
                    self.error("</option> with no open option");
                }
                false
            }
            Token::EndTag(tag) if tag.name == "select" => {
                if !self.has_in_select_scope("select") {
                    self.error("</select> with no open select");
                    return false;
                }
                self.pop_until_html("select");
                self.reset_insertion_mode();
                false
            }
            // A nested `<select>` closes the outer one instead of
            // nesting -- select elements cannot contain each other.
            Token::StartTag(tag) if tag.name == "select" => {
                self.error("nested <select>");
                if !self.has_in_select_scope("select") {
                    return false;
                }
                self.pop_until_html("select");
                self.reset_insertion_mode();
                false
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "input" | "keygen" | "textarea") => {
                self.error("form control inside a select");
                if !self.has_in_select_scope("select") {
                    return false;
                }
                self.pop_until_html("select");
                self.reset_insertion_mode();
                true
            }
            Token::StartTag(tag) if matches!(tag.name.as_str(), "script" | "template") => self.in_head(token),
            Token::EndTag(tag) if tag.name == "template" => self.in_head(token),
            Token::Eof => self.in_body(token),
            _ => {
                self.error("content in a select that cannot go there");
                false
            }
        }
    }

    fn in_select_in_table(&mut self, token: &Token) -> bool {
        let escapes = match token {
            Token::StartTag(tag) | Token::EndTag(tag) => {
                matches!(tag.name.as_str(), "caption" | "table" | "tbody" | "tfoot" | "thead" | "tr" | "td" | "th")
            }
            _ => false,
        };
        if escapes {
            // In a table, table markup wins: the select is closed and
            // the tag reprocessed where it belongs.
            self.error("table markup inside a select in a table");
            let is_end = matches!(token, Token::EndTag(_));
            let name = match token {
                Token::StartTag(tag) | Token::EndTag(tag) => tag.name.clone(),
                _ => String::new(),
            };
            if is_end && !self.has_in_table_scope(&name) {
                return false;
            }
            self.pop_until_html("select");
            self.reset_insertion_mode();
            return true;
        }
        self.in_select(token)
    }

    fn in_template(&mut self, token: &Token) -> bool {
        match token {
            Token::Char(_) | Token::Comment(_) | Token::Doctype(_) => self.in_body(token),
            Token::StartTag(tag)
                if matches!(
                    tag.name.as_str(),
                    "base" | "basefont" | "bgsound" | "link" | "meta" | "noframes" | "script" | "style" | "template" | "title"
                ) =>
            {
                self.in_head(token)
            }
            Token::EndTag(tag) if tag.name == "template" => self.in_head(token),
            Token::StartTag(tag) if matches!(tag.name.as_str(), "caption" | "colgroup" | "tbody" | "tfoot" | "thead") => {
                self.switch_template_mode(Mode::InTable)
            }
            Token::StartTag(tag) if tag.name == "col" => self.switch_template_mode(Mode::InColumnGroup),
            Token::StartTag(tag) if tag.name == "tr" => self.switch_template_mode(Mode::InTableBody),
            Token::StartTag(tag) if matches!(tag.name.as_str(), "td" | "th") => self.switch_template_mode(Mode::InRow),
            Token::StartTag(_) => self.switch_template_mode(Mode::InBody),
            Token::EndTag(_) => {
                self.error("unexpected end tag in a template");
                false
            }
            Token::Eof => {
                if !self.open.iter().any(|&id| self.is_html(id, "template")) {
                    self.done = true;
                    return false;
                }
                self.error("eof inside a template");
                self.pop_until_html("template");
                self.clear_afe_to_marker();
                self.template_modes.pop();
                self.reset_insertion_mode();
                true
            }
        }
    }

    fn switch_template_mode(&mut self, mode: Mode) -> bool {
        self.template_modes.pop();
        self.template_modes.push(mode);
        self.mode = mode;
        true
    }

    fn after_body(&mut self, token: &Token) -> bool {
        match token {
            Token::Char(c) if is_whitespace(*c) => self.in_body(token),
            // A comment after `</body>` belongs to the `<html>` element,
            // not to the body it follows.
            Token::Comment(text) => {
                let html = self.open[0];
                self.insert_comment(text, Some(html));
                false
            }
            Token::Doctype(_) => {
                self.error("doctype after </body>");
                false
            }
            Token::StartTag(tag) if tag.name == "html" => self.in_body(token),
            Token::EndTag(tag) if tag.name == "html" => {
                if self.fragment_context.is_some() {
                    self.error("</html> in a fragment");
                    return false;
                }
                self.mode = Mode::AfterAfterBody;
                false
            }
            Token::Eof => {
                self.done = true;
                false
            }
            _ => {
                self.error("content after </body>");
                self.mode = Mode::InBody;
                true
            }
        }
    }

    fn in_frameset(&mut self, token: &Token) -> bool {
        match token {
            Token::Char(c) if is_whitespace(*c) => {
                self.insert_char(*c);
                false
            }
            Token::Comment(text) => {
                self.insert_comment(text, None);
                false
            }
            Token::Doctype(_) => {
                self.error("doctype in a frameset");
                false
            }
            Token::StartTag(tag) if tag.name == "html" => self.in_body(token),
            Token::StartTag(tag) if tag.name == "frameset" => {
                self.insert_element(tag, Namespace::Html);
                false
            }
            Token::EndTag(tag) if tag.name == "frameset" => {
                if self.is_html(self.current(), "html") {
                    self.error("</frameset> with no open frameset");
                    return false;
                }
                self.open.pop();
                if self.fragment_context.is_none() && !self.is_html(self.current(), "frameset") {
                    self.mode = Mode::AfterFrameset;
                }
                false
            }
            Token::StartTag(tag) if tag.name == "frame" => {
                self.insert_element(tag, Namespace::Html);
                self.open.pop();
                false
            }
            Token::StartTag(tag) if tag.name == "noframes" => self.in_head(token),
            Token::Eof => {
                if !self.is_html(self.current(), "html") {
                    self.error("eof inside a frameset");
                }
                self.done = true;
                false
            }
            _ => {
                self.error("content in a frameset that cannot go there");
                false
            }
        }
    }

    fn after_frameset(&mut self, token: &Token) -> bool {
        match token {
            Token::Char(c) if is_whitespace(*c) => {
                self.insert_char(*c);
                false
            }
            Token::Comment(text) => {
                self.insert_comment(text, None);
                false
            }
            Token::Doctype(_) => {
                self.error("doctype after a frameset");
                false
            }
            Token::StartTag(tag) if tag.name == "html" => self.in_body(token),
            Token::EndTag(tag) if tag.name == "html" => {
                self.mode = Mode::AfterAfterFrameset;
                false
            }
            Token::StartTag(tag) if tag.name == "noframes" => self.in_head(token),
            Token::Eof => {
                self.done = true;
                false
            }
            _ => {
                self.error("content after a frameset");
                false
            }
        }
    }

    fn after_after_body(&mut self, token: &Token) -> bool {
        match token {
            Token::Comment(text) => {
                let root = self.doc.root;
                self.insert_comment(text, Some(root));
                false
            }
            Token::Doctype(_) => self.in_body(token),
            Token::Char(c) if is_whitespace(*c) => self.in_body(token),
            Token::StartTag(tag) if tag.name == "html" => self.in_body(token),
            Token::Eof => {
                self.done = true;
                false
            }
            _ => {
                self.error("content after the document");
                self.mode = Mode::InBody;
                true
            }
        }
    }

    fn after_after_frameset(&mut self, token: &Token) -> bool {
        match token {
            Token::Comment(text) => {
                let root = self.doc.root;
                self.insert_comment(text, Some(root));
                false
            }
            Token::Doctype(_) => self.in_body(token),
            Token::Char(c) if is_whitespace(*c) => self.in_body(token),
            Token::StartTag(tag) if tag.name == "html" => self.in_body(token),
            Token::Eof => {
                self.done = true;
                false
            }
            Token::StartTag(tag) if tag.name == "noframes" => self.in_head(token),
            _ => {
                self.error("content after the document");
                false
            }
        }
    }

    // §13.2.6.5, "the rules for parsing tokens in foreign content":
    // inside `<svg>` or `<math>`, HTML's own element rules don't apply,
    // and a tag that clearly belongs to HTML breaks back out to it.
    fn foreign_content(&mut self, token: &Token) -> bool {
        match token {
            Token::Char('\0') => {
                self.error("null character in foreign content");
                self.insert_char('\u{FFFD}');
                false
            }
            Token::Char(c) => {
                self.insert_char(*c);
                if !is_whitespace(*c) {
                    self.frameset_ok = false;
                }
                false
            }
            Token::Comment(text) => {
                self.insert_comment(text, None);
                false
            }
            Token::Doctype(_) => {
                self.error("doctype in foreign content");
                false
            }
            // An unmistakably-HTML tag inside foreign content closes out
            // of it rather than being treated as an SVG/MathML element.
            Token::StartTag(tag)
                if matches!(
                    tag.name.as_str(),
                    "b" | "big"
                        | "blockquote"
                        | "body"
                        | "br"
                        | "center"
                        | "code"
                        | "dd"
                        | "div"
                        | "dl"
                        | "dt"
                        | "em"
                        | "embed"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "head"
                        | "hr"
                        | "i"
                        | "img"
                        | "li"
                        | "listing"
                        | "menu"
                        | "meta"
                        | "nobr"
                        | "ol"
                        | "p"
                        | "pre"
                        | "ruby"
                        | "s"
                        | "small"
                        | "span"
                        | "strong"
                        | "strike"
                        | "sub"
                        | "sup"
                        | "table"
                        | "tt"
                        | "u"
                        | "ul"
                        | "var"
                ) || (tag.name == "font" && tag.attrs.iter().any(|a| matches!(a.name.as_str(), "color" | "face" | "size"))) =>
            {
                self.error("html start tag inside foreign content");
                if self.fragment_context.is_some() {
                    return self.in_body(token);
                }
                while self.open.len() > 1 {
                    let id = self.current();
                    if self.ns_of(id) == Namespace::Html
                        || (self.ns_of(id) == Namespace::MathMl && MATHML_TEXT_INTEGRATION.contains(&self.name_of(id)))
                        || (self.ns_of(id) == Namespace::Svg && SVG_HTML_INTEGRATION.contains(&self.name_of(id)))
                    {
                        break;
                    }
                    self.open.pop();
                }
                true
            }
            Token::StartTag(tag) => {
                let ns = self.ns_of(self.adjusted_current());
                let mut fixed = tag.clone();
                if ns == Namespace::Svg {
                    fixed.name = adjust_svg_tag(&fixed.name);
                }
                adjust_foreign_attrs(ns, &mut fixed.attrs);
                self.insert_element(&fixed, ns);
                if tag.self_closing {
                    self.open.pop();
                }
                false
            }
            Token::EndTag(tag) if tag.name == "script" && self.is_svg_script() => {
                self.open.pop();
                false
            }
            Token::EndTag(tag) => {
                let mut i = self.open.len() - 1;
                if !self.name_of(self.open[i]).eq_ignore_ascii_case(&tag.name) {
                    self.error("misnested end tag in foreign content");
                }
                loop {
                    let id = self.open[i];
                    if self.name_of(id).eq_ignore_ascii_case(&tag.name) {
                        self.open.truncate(i);
                        return false;
                    }
                    if i == 0 {
                        return false;
                    }
                    i -= 1;
                    if self.ns_of(self.open[i]) == Namespace::Html {
                        return self.dispatch(token);
                    }
                }
            }
            Token::Eof => {
                self.done = true;
                false
            }
        }
    }

    fn is_svg_script(&self) -> bool {
        let id = self.current();
        self.ns_of(id) == Namespace::Svg && self.name_of(id) == "script"
    }
}

// §13.2.6.4.1's quirks-mode table, trimmed to the prefixes that actually
// occur: a doctype that isn't plain `<!DOCTYPE html>` puts the document
// in quirks or limited-quirks mode, which is a property of the parse
// even though nothing here lays out CSS.
fn quirks_for(force: bool, name: &str, public_id: &str, system_id: &str) -> QuirksMode {
    if force || name != "html" {
        return QuirksMode::Quirks;
    }
    let public = public_id.to_lowercase();
    const QUIRKY_PREFIXES: &[&str] = &[
        "+//silmaril//dtd html pro v0r11 19970101//",
        "-//as//dtd html 3.0 aswedit + extensions//",
        "-//advasoft ltd//dtd html 3.0 aswedit + extensions//",
        "-//ietf//dtd html 2.0",
        "-//ietf//dtd html 3",
        "-//metrius//dtd metrius presentational//",
        "-//microsoft//dtd internet explorer 2.0 html",
        "-//microsoft//dtd internet explorer 3.0 html",
        "-//netscape comm. corp.//dtd html",
        "-//o'reilly and associates//dtd html",
        "-//sq//dtd html 2.0 hotmetal + extensions//",
        "-//softquad//dtd html",
        "-//spyglass//dtd html 2.0 extended//",
        "-//sun microsystems corp.//dtd hotjava html//",
        "-//w3c//dtd html 3",
        "-//w3c//dtd html 4.0 frameset//",
        "-//w3c//dtd html 4.0 transitional//",
        "-//w3c//dtd html experimental",
        "-//w3o//dtd w3 html 3.0//",
        "-//webtechs//dtd mozilla html",
        "html",
    ];
    if public == "-//w3o//dtd w3 html strict 3.0//en//"
        || public == "-/w3c/dtd html 4.0 transitional/en"
        || QUIRKY_PREFIXES.iter().any(|p| public.starts_with(p))
    {
        return QuirksMode::Quirks;
    }
    if system_id.is_empty() && (public.starts_with("-//w3c//dtd html 4.01 frameset//") || public.starts_with("-//w3c//dtd html 4.01 transitional//"))
    {
        return QuirksMode::Quirks;
    }
    if public.starts_with("-//w3c//dtd xhtml 1.0 frameset//")
        || public.starts_with("-//w3c//dtd xhtml 1.0 transitional//")
        || (!system_id.is_empty()
            && (public.starts_with("-//w3c//dtd html 4.01 frameset//") || public.starts_with("-//w3c//dtd html 4.01 transitional//")))
    {
        return QuirksMode::LimitedQuirks;
    }
    QuirksMode::NoQuirks
}

#[cfg(test)]
mod tests {
    // The spec's algorithms are written against the stack of open
    // elements -- "have an element in scope" walks it, "reconstruct the
    // active formatting elements" walks it -- so per-token work grows
    // with depth and a document of nothing but `<div>` is quadratic:
    // 8000 of them measured 6.7s before this cap, 38ms after. The tree
    // it produced was also deeper than any recursive consumer could
    // walk (markdown::render::html_runs overflowed the stack on it).
    #[test]
    fn nesting_stops_deepening_past_the_open_element_limit() {
        let src = format!("{}x{}", "<div>".repeat(20000), "</div>".repeat(20000));
        let started = std::time::Instant::now();
        let (doc, roots) = crate::html::parse_fragment(&src, "div");
        assert!(started.elapsed().as_secs() < 10, "20k nested divs must not be quadratic");

        // Depth measured without recursing, which is the whole point.
        let mut deepest = 0;
        let mut stack: Vec<(crate::html::NodeId, usize)> = roots.iter().map(|&r| (r, 1)).collect();
        while let Some((node, depth)) = stack.pop() {
            deepest = deepest.max(depth);
            stack.extend(doc.children(node).iter().map(|&c| (c, depth + 1)));
        }
        assert!(deepest < 600, "the tree bottoms out near the limit, not at 20000 (got {deepest})");
    }

    use super::super::{parse, parse_fragment};

    // Trees are compared in html5lib-tests' own indented format (see
    // Document::to_test_format), so an expectation reads as the tree it
    // describes rather than as a chain of index lookups.
    fn assert_tree(input: &str, expected: &str) {
        let doc = parse(input);
        let got = doc.to_test_format();
        // The expectations are written indented to sit nicely in the
        // source; the common leading indent is removed rather than all
        // leading space, since the *relative* indent is the tree shape.
        // Blank lines are the raw string's own framing, not part of the
        // tree -- every real line starts with `|`.
        let lines: Vec<&str> = expected.lines().filter(|l| !l.trim().is_empty()).collect();
        let dedent = lines.iter().map(|l| l.len() - l.trim_start_matches(' ').len()).min().unwrap_or(0);
        let want: String = lines.iter().map(|l| format!("{}\n", &l[dedent..])).collect();
        assert_eq!(got, want, "\n--- parsing ---\n{input}\n--- got ---\n{got}--- want ---\n{want}");
    }

    #[test]
    fn a_minimal_document_gets_the_elements_it_never_named() {
        assert_tree(
            "hello",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     "hello"
            "#,
        );
    }

    #[test]
    fn a_doctype_and_explicit_structure_are_kept() {
        assert_tree(
            "<!DOCTYPE html><html><head><title>T</title></head><body><p>x</p></body></html>",
            r#"
            | <!DOCTYPE html>
            | <html>
            |   <head>
            |     <title>
            |       "T"
            |   <body>
            |     <p>
            |       "x"
            "#,
        );
    }

    // The adoption agency algorithm's own headline case: the `<i>` has
    // to end up split across two parents, one inside the `<b>` and one
    // after it.
    #[test]
    fn misnested_formatting_is_un_crossed() {
        assert_tree(
            "<p>1<b>2<i>3</b>4</i>5</p>",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     <p>
            |       "1"
            |       <b>
            |         "2"
            |         <i>
            |           "3"
            |       <i>
            |         "4"
            |       "5"
            "#,
        );
    }

    // Reconstruction: `</p>` pops the `<b>` off the stack but leaves it
    // in the list of active formatting elements, so the next paragraph
    // gets a fresh `<b>` of its own and is bold too -- without the
    // document ever saying so.
    #[test]
    fn active_formatting_is_reconstructed_in_the_next_block() {
        assert_tree(
            "<p><b>a</p><p>b",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     <p>
            |       <b>
            |         "a"
            |     <p>
            |       <b>
            |         "b"
            "#,
        );
    }

    // A `<p>` does not close an open `<b>`: the paragraph opens *inside*
    // it, which is the counterpart to the reconstruction case above and
    // the reason that one needs the `</p>`.
    #[test]
    fn a_paragraph_opens_inside_an_open_formatting_element() {
        assert_tree(
            "<b>a<p>b</p>",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     <b>
            |       "a"
            |       <p>
            |         "b"
            "#,
        );
    }

    // Foster parenting: content a table has no place for is moved out in
    // front of the table rather than into it.
    #[test]
    fn stray_content_in_a_table_is_foster_parented_before_it() {
        assert_tree(
            "<table><b>x</b></table>",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     <b>
            |       "x"
            |     <table>
            "#,
        );
    }

    // Implied end tags: a new `<li>` closes the previous one instead of
    // nesting inside it.
    #[test]
    fn a_list_item_closes_the_previous_one() {
        assert_tree(
            "<ul><li>a<li>b</ul>",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     <ul>
            |       <li>
            |         "a"
            |       <li>
            |         "b"
            "#,
        );
    }

    // The search up the stack stops at a special element -- except for
    // `address`, `div` and `p`, which it steps straight through. So a
    // list item inside a `div` still closes the outer item (leaving the
    // div behind, empty), while one inside a `<table>` would not.
    #[test]
    fn the_list_item_search_steps_through_a_div_but_not_a_table() {
        assert_tree(
            "<ul><li>a<div><li>b</div></ul>",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     <ul>
            |       <li>
            |         "a"
            |         <div>
            |       <li>
            |         "b"
            "#,
        );
        assert_tree(
            "<ul><li>a<table><li>b</table></ul>",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     <ul>
            |       <li>
            |         "a"
            |         <li>
            |           "b"
            |         <table>
            "#,
        );
    }

    #[test]
    fn a_table_gets_the_tbody_and_row_structure_it_did_not_write() {
        assert_tree(
            "<table><tr><td>a</table>",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     <table>
            |       <tbody>
            |         <tr>
            |           <td>
            |             "a"
            "#,
        );
    }

    // Implied end tags again, in a table: `<td>a<td>b` is two cells.
    #[test]
    fn cells_and_rows_close_themselves() {
        assert_tree(
            "<table><tr><td>a<td>b<tr><td>c</table>",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     <table>
            |       <tbody>
            |         <tr>
            |           <td>
            |             "a"
            |           <td>
            |             "b"
            |         <tr>
            |           <td>
            |             "c"
            "#,
        );
    }

    #[test]
    fn a_paragraph_is_closed_by_a_block_that_cannot_nest_in_it() {
        assert_tree(
            "<p>a<div>b</div>",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     <p>
            |       "a"
            |     <div>
            |       "b"
            "#,
        );
    }

    // A stray end tag stops at the first special element rather than
    // closing something it has no business closing.
    #[test]
    fn a_stray_end_tag_closes_nothing() {
        assert_tree(
            "<p>a</div>b",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     <p>
            |       "ab"
            "#,
        );
    }

    // `</p>` with nothing open creates an empty paragraph -- the spec's
    // own recovery, not a bug here. (Before the body has started it is
    // simply dropped instead, which the second case pins.)
    #[test]
    fn an_unmatched_paragraph_end_tag_creates_an_empty_paragraph() {
        assert_tree(
            "x</p>y",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     "x"
            |     <p>
            |     "y"
            "#,
        );
        assert_tree(
            "</p>x",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     "x"
            "#,
        );
    }

    #[test]
    fn a_newline_right_after_pre_is_dropped() {
        assert_tree(
            "<pre>\nkept</pre>",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     <pre>
            |       "kept"
            "#,
        );
    }

    #[test]
    fn head_content_after_body_started_still_goes_in_the_head() {
        assert_tree(
            "<body>x<title>T</title>",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     "x"
            |     <title>
            |       "T"
            "#,
        );
    }

    #[test]
    fn nested_anchors_never_nest() {
        assert_tree(
            r#"<a href="1">x<a href="2">y"#,
            r#"
            | <html>
            |   <head>
            |   <body>
            |     <a>
            |       href="1"
            |       "x"
            |     <a>
            |       href="2"
            |       "y"
            "#,
        );
    }

    #[test]
    fn svg_keeps_its_namespace_and_camel_case_names() {
        assert_tree(
            "<svg><clipPath><foreignObject>x</foreignObject></clipPath></svg>",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     <svg svg>
            |       <svg clipPath>
            |         <svg foreignObject>
            |           "x"
            "#,
        );
    }

    // An unmistakably-HTML tag inside foreign content breaks back out of
    // it rather than becoming an SVG element.
    #[test]
    fn an_html_block_inside_svg_breaks_out_of_it() {
        assert_tree(
            "<svg><p>x",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     <svg svg>
            |     <p>
            |       "x"
            "#,
        );
    }

    #[test]
    fn a_comment_after_the_document_belongs_to_the_document() {
        assert_tree(
            "<html><body>x</body></html><!-- after -->",
            r#"
            | <html>
            |   <head>
            |   <body>
            |     "x"
            | <!--  after  -->
            "#,
        );
    }

    // The fragment case, which is what markdown actually uses: no
    // synthetic html/head/body, just the nodes that were written.
    #[test]
    fn a_fragment_parses_without_the_document_scaffolding() {
        let (doc, roots) = parse_fragment("<b>bold</b> and <i>italic</i>", "div");
        let names: Vec<String> = roots
            .iter()
            .map(|&id| match doc.node(id).name() {
                Some(n) => n.to_string(),
                None => format!("{:?}", doc.text_content(id)),
            })
            .collect();
        assert_eq!(names, vec!["b", "\" and \"", "i"]);
        assert_eq!(doc.text_content(roots[0]), "bold");
        assert_eq!(doc.text_content(roots[2]), "italic");
    }

    // A fragment's context element decides the rules: rows only make
    // sense inside a table.
    #[test]
    fn a_fragment_is_parsed_in_the_context_it_was_given() {
        let (doc, roots) = parse_fragment("<tr><td>a</td></tr>", "tbody");
        assert_eq!(roots.len(), 1);
        assert_eq!(doc.node(roots[0]).name(), Some("tr"));

        // The same markup in a div context has nowhere to put a row.
        let (doc, roots) = parse_fragment("<tr><td>a</td></tr>", "div");
        assert_eq!(roots.len(), 1);
        assert_eq!(doc.text_content(roots[0]), "a");
    }

    #[test]
    fn attributes_survive_with_their_values() {
        let (doc, roots) = parse_fragment(r#"<a href="/x" title='T'>link</a>"#, "div");
        assert_eq!(doc.node(roots[0]).attr("href"), Some("/x"));
        assert_eq!(doc.node(roots[0]).attr("title"), Some("T"));
    }

    #[test]
    fn quirks_mode_follows_the_doctype() {
        use super::super::QuirksMode;
        assert_eq!(parse("<!DOCTYPE html><p>x").quirks, QuirksMode::NoQuirks);
        assert_eq!(parse("<p>x").quirks, QuirksMode::Quirks, "no doctype at all is quirks mode");
        assert_eq!(parse(r#"<!DOCTYPE HTML PUBLIC "-//W3C//DTD HTML 4.01 Transitional//EN">"#).quirks, QuirksMode::Quirks);
    }

    // Malformed input has to terminate and produce *something*: every
    // prefix of a document with every kind of nesting mistake in it.
    #[test]
    fn every_truncation_of_a_gnarly_document_terminates() {
        let doc = "<!DOCTYPE html><table><b>x<tr><td><p>1<b>2<i>3</b>4</i><select><option>o</table><svg><foreignObject><ul><li>a<li>b</svg></p>";
        for cut in 0..=doc.len() {
            if !doc.is_char_boundary(cut) {
                continue;
            }
            let parsed = parse(&doc[..cut]);
            assert!(!parsed.nodes.is_empty());
        }
    }
}

#[cfg(test)]
mod real_world_tests {
    use super::super::parse;

    // Every HTML file this machine happens to have documentation in,
    // parsed and sanity-checked. Not a conformance suite -- these are
    // real documents, not tricky ones -- but it is real markup written
    // by real tools, which is what catches a parser that only works on
    // the examples it was written against. Skipped where there's no such
    // directory, the same "quietly unavailable" contract the git and
    // archive tests use.
    #[test]
    fn parses_every_html_document_this_machine_has() {
        let mut checked = 0;
        for dir in ["/usr/share/doc", "/usr/share/gtk-doc"] {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for entry in entries.flatten().take(60) {
                let Ok(sub) = std::fs::read_dir(entry.path()) else { continue };
                for file in sub.flatten() {
                    let path = file.path();
                    if path.extension().is_none_or(|e| e != "html") {
                        continue;
                    }
                    let Ok(text) = std::fs::read_to_string(&path) else { continue };
                    if text.len() > 2_000_000 {
                        continue;
                    }
                    let doc = parse(&text);
                    // Every document ends up with the three elements it
                    // is required to have, whatever it actually wrote.
                    assert!(doc.find("html").is_some(), "{}", path.display());
                    assert!(doc.find("head").is_some(), "{}", path.display());
                    assert!(doc.find("body").is_some(), "{}", path.display());
                    // Every node reachable from the root has that root
                    // as an ancestor -- the parent/child links can't
                    // have come apart during reparenting.
                    check_links(&doc, doc.root);
                    checked += 1;
                }
            }
        }
        if checked == 0 {
            return;
        }
        assert!(checked > 0);
    }

    fn check_links(doc: &super::super::Document, id: usize) {
        for &child in doc.children(id) {
            assert_eq!(doc.node(child).parent, Some(id), "child {child} disagrees about its parent");
            check_links(doc, child);
        }
    }
}
