// Tabular display for delimiter-separated files: a CSV's columns line up
// on screen without any character of the file changing. Only where
// padding goes between characters is decided here; this module never gets
// a mutable buffer.
//
// So a screen column and a character index differ, as they do for a wide
// glyph, and the editor converts between them with the maps built here.

use super::unicode_width::char_width;

// How a language's tabular form is shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub delimiter: char,
    pub kind: Kind,
    pub escape: Escape,
}

// How a delimiter that is content rather than a boundary is written.
// Without this, such a row reads as having an extra column, and the table
// aligns against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escape {
    // RFC 4180: a field that opens with `"` runs to its closing quote,
    // and `""` inside it is an escaped quote.
    Quotes,
    // Markdown: a backslash makes what follows it content. `\|` is the only
    // way to write a pipe in a cell.
    Backslash,
}

// What the delimiter *is* to the row, which is what decides where
// padding may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    // `a,b`: the delimiter ends the field before it. Padding goes after the
    // delimiter, plus one space, since the row has no spaces of its own.
    Terminated,
    // `| a | b |`: the delimiters frame the cells. Padding goes before the
    // delimiter so the frame lines up, with no extra gap since the cells
    // carry their own spaces.
    Framed,
}

impl Kind {
    // The minimum separation beyond the widest field.
    fn gap(self) -> usize {
        match self {
            Kind::Terminated => 1,
            Kind::Framed => 0,
        }
    }

    // Whether a row of rule characters is left out of the column widths.
    // Markdown's `|:---|---:|` is: padding can only add spaces, not stretch
    // the dashes, so a long dash run would widen the column for nothing. In
    // a CSV a row of dashes is data.
    fn rule_rows(self) -> bool {
        matches!(self, Kind::Framed)
    }
}

// The style of a language's tabular form, or `None` for most languages,
// which is why the `tabular` bishopt can default to matching everything.
pub fn style(language: &str) -> Option<Style> {
    match language {
        "csv" => Some(Style { delimiter: ',', kind: Kind::Terminated, escape: Escape::Quotes }),
        // A real `.tsv` holds literal tabs, which the editor's one char per
        // column rendering can't place correctly anyway (see
        // `run_insert_mode`).
        "tsv" | "tab" => Some(Style { delimiter: '\t', kind: Kind::Terminated, escape: Escape::Quotes }),
        "psv" => Some(Style { delimiter: '|', kind: Kind::Terminated, escape: Escape::Quotes }),
        "markdown" => Some(Style { delimiter: '|', kind: Kind::Framed, escape: Escape::Backslash }),
        _ => None,
    }
}

// A row of nothing but rule characters (`|:---|---:|`). Whole lines only:
// a field of dashes in an ordinary row is content.
fn is_rule_row(line: &[char]) -> bool {
    !line.is_empty() && line.iter().any(|c| *c == '-') && line.iter().all(|c| matches!(c, '-' | ':' | '|' | ' ' | '\t'))
}

// A column wider than this is clamped for alignment (the text isn't cut;
// the row runs long), and columns past the limit aren't aligned.
const MAX_COLUMN_WIDTH: usize = 60;
const MAX_COLUMNS: usize = 128;

// An independently aligned run of lines and its column widths. A CSV is
// one region; a markdown document has one per table, since unrelated
// tables shouldn't widen each other.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Region {
    lines: std::ops::Range<usize>,
    widths: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    style: Style,
    regions: Vec<Region>,
}

impl Layout {
    pub fn style(&self) -> Style {
        self.style
    }

    pub fn regions(&self) -> usize {
        self.regions.len()
    }

    fn widths_for(&self, line: usize) -> Option<&[usize]> {
        self.regions.iter().find(|r| r.lines.contains(&line)).map(|r| r.widths.as_slice())
    }
}

// The whole file as one region, as a CSV wants.
pub fn measure(lines: &[&[char]], style: Style) -> Layout {
    measure_regions(lines, style, &[0..lines.len()])
}

// The widest field per column, per region, across all its lines rather
// than the visible ones, so widths don't change as you scroll. A line in
// no region is left alone.
pub fn measure_regions(lines: &[&[char]], style: Style, regions: &[std::ops::Range<usize>]) -> Layout {
    let mut out = Vec::new();
    for region in regions {
        let mut widths: Vec<usize> = Vec::new();
        for line in lines[region.start.min(lines.len())..region.end.min(lines.len())].iter() {
            if style.kind.rule_rows() && is_rule_row(line) {
                continue;
            }
            for (i, field) in fields(line, style).into_iter().enumerate() {
                if i >= MAX_COLUMNS {
                    break;
                }
                let width = line[field.start..field.end].iter().map(|c| char_width(*c)).sum::<usize>();
                if i == widths.len() {
                    widths.push(0);
                }
                widths[i] = widths[i].max(width).min(MAX_COLUMN_WIDTH);
            }
        }
        out.push(Region { lines: region.clone(), widths });
    }
    Layout { style, regions: out }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub start: usize,
    pub end: usize,
}

// One line's fields as character ranges, honouring the style's escape so
// a delimiter meant as content doesn't split a field: a quoted address with
// a comma is one column.
pub fn fields(line: &[char], style: Style) -> Vec<Field> {
    let delimiter = style.delimiter;
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut quoted = false;
    while i < line.len() {
        let c = line[i];
        // What follows a backslash is content, including a second backslash,
        // which leaves a delimiter after the pair a delimiter.
        if style.escape == Escape::Backslash && c == '\\' {
            i += 2;
            continue;
        }
        if quoted {
            if c == '"' {
                // A doubled quote is an escaped one.
                if line.get(i + 1) == Some(&'"') {
                    i += 2;
                    continue;
                }
                quoted = false;
            }
            i += 1;
            continue;
        }
        if style.escape == Escape::Quotes && c == '"' && i == start {
            quoted = true;
            i += 1;
            continue;
        }
        if c == delimiter {
            out.push(Field { start, end: i });
            i += 1;
            start = i;
            continue;
        }
        i += 1;
    }
    out.push(Field { start, end: line.len() });
    out
}

// A line rendered for display: the characters to draw, and the two maps
// between a character of the line and the cell it occupies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    // The line's characters with padding spliced in.
    pub cells: Vec<char>,
    // For each cell, the character it came from; `None` for padding.
    pub source_at: Vec<Option<usize>>,
    // For each character, the cell it starts at. One longer than the line,
    // for the cursor at end of line.
    pub cell_of: Vec<usize>,
}

impl Row {
    // The identity row, for a language with no tabular form, so the editor
    // has one code path.
    pub fn plain(line: &[char]) -> Row {
        Row { cells: line.to_vec(), source_at: (0..line.len()).map(Some).collect(), cell_of: (0..=line.len()).collect() }
    }
}

// `line` padded so each column starts where the layout says, before or
// after each delimiter as the style decides. A field wider than its
// clamped column pushes the rest of the row along rather than being cut.
pub fn row(line: &[char], line_number: usize, layout: &Layout) -> Row {
    // A line outside every region (prose around a markdown table) is drawn
    // as it is.
    let Some(widths) = layout.widths_for(line_number) else { return Row::plain(line) };
    let fields = fields(line, layout.style);
    let mut cells = Vec::with_capacity(line.len());
    let mut source_at: Vec<Option<usize>> = Vec::with_capacity(line.len());
    let mut cell_of = vec![0usize; line.len() + 1];
    // A line with no delimiter (a comment, a blank line) isn't a table row.
    if fields.len() <= 1 {
        return Row::plain(line);
    }
    for (index, field) in fields.iter().enumerate() {
        for i in field.start..field.end {
            cell_of[i] = cells.len();
            cells.push(line[i]);
            source_at.push(Some(i));
        }
        // The delimiter and the padding, in the style's order. The last
        // field has neither.
        if field.end < line.len() {
            let width: usize = line[field.start..field.end].iter().map(|c| char_width(*c)).sum();
            let target = widths.get(index).copied().unwrap_or(width);
            let pad = target.saturating_sub(width) + layout.style.kind.gap();
            let (before, after) = match layout.style.kind {
                Kind::Framed => (pad, 0),
                Kind::Terminated => (0, pad),
            };
            for _ in 0..before {
                cells.push(' ');
                source_at.push(None);
            }
            cell_of[field.end] = cells.len();
            cells.push(line[field.end]);
            source_at.push(Some(field.end));
            for _ in 0..after {
                cells.push(' ');
                source_at.push(None);
            }
        }
    }
    cell_of[line.len()] = cells.len();
    Row { cells, source_at, cell_of }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    fn split(line: &str, style: Style) -> Vec<String> {
        let cs = chars(line);
        fields(&cs, style).iter().map(|f| cs[f.start..f.end].iter().collect()).collect()
    }

    #[test]
    fn a_language_without_a_tabular_form_has_no_style() {
        assert_eq!(style("csv").map(|s| s.delimiter), Some(','));
        assert_eq!(style("tsv").map(|s| s.delimiter), Some('\t'));
        assert_eq!(style("markdown").map(|s| s.delimiter), Some('|'));
        assert_eq!(style("rust"), None);
    }

    // A CSV row carries no spaces of its own; a markdown row does.
    #[test]
    fn csv_terminates_its_fields_and_markdown_frames_them() {
        assert_eq!(style("csv").unwrap().kind, Kind::Terminated);
        assert_eq!(style("markdown").unwrap().kind, Kind::Framed);
    }

    #[test]
    fn fields_split_on_the_delimiter() {
        assert_eq!(split("a,b,c", csv()), vec!["a", "b", "c"]);
        assert_eq!(split("a,,c", csv()), vec!["a", "", "c"]);
        assert_eq!(split("", csv()), vec![""]);
        assert_eq!(split("solo", csv()), vec!["solo"]);
    }

    // A quoted field holding a delimiter is one column.
    #[test]
    fn a_delimiter_inside_quotes_does_not_split() {
        assert_eq!(split("a,\"b,c\",d", csv()), vec!["a", "\"b,c\"", "d"]);
        assert_eq!(split("\"one, two\"", csv()), vec!["\"one, two\""]);
    }

    #[test]
    fn a_doubled_quote_is_an_escaped_quote_and_stays_inside() {
        assert_eq!(split("\"say \"\"hi\"\", ok\",next", csv()), vec!["\"say \"\"hi\"\", ok\"", "next"]);
    }

    #[test]
    fn a_quote_mid_field_does_not_open_a_quoted_field() {
        assert_eq!(split("a\"b,c", csv()), vec!["a\"b", "c"]);
    }

    // `\|` is the only way to write a pipe in a markdown cell, so a row
    // using one has fewer columns than pipes.
    #[test]
    fn an_escaped_pipe_does_not_split_a_markdown_row() {
        assert_eq!(split(r"| a \| b | c |", md()), vec!["", r" a \| b ", " c ", ""]);
        assert_eq!(split(r"| one `${s/:/\|}` | 2.13 |", md()), vec!["", r" one `${s/:/\|}` ", " 2.13 ", ""]);
    }

    #[test]
    fn a_doubled_backslash_leaves_the_pipe_after_it_a_delimiter() {
        assert_eq!(split(r"| a \\| b |", md()), vec!["", r" a \\", " b ", ""]);
    }

    // The escapes aren't interchangeable: a CSV quotes (RFC 4180), and in
    // markdown a `"` opening a cell is just a character.
    #[test]
    fn each_style_has_only_its_own_escape() {
        assert_eq!(split(r"a\,b", csv()), vec![r"a\", "b"]);
        assert_eq!(split("|\"a | b\"|", md()), vec!["", "\"a ", " b\"", ""]);
    }

    fn csv() -> Style {
        style("csv").unwrap()
    }

    fn one(line: &[char], style: Style) -> Layout {
        measure(&[line], style)
    }

    fn rendered(lines: &[&str], style: Style) -> Vec<String> {
        let all: Vec<Vec<char>> = lines.iter().map(|l| chars(l)).collect();
        let refs: Vec<&[char]> = all.iter().map(|l| l.as_slice()).collect();
        let layout = measure(&refs, style);
        all.iter().enumerate().map(|(i, l)| row(l, i, &layout).cells.iter().collect()).collect()
    }

    fn rendered_regions(lines: &[&str], style: Style, regions: &[std::ops::Range<usize>]) -> Vec<String> {
        let all: Vec<Vec<char>> = lines.iter().map(|l| chars(l)).collect();
        let refs: Vec<&[char]> = all.iter().map(|l| l.as_slice()).collect();
        let layout = measure_regions(&refs, style, regions);
        all.iter().enumerate().map(|(i, l)| row(l, i, &layout).cells.iter().collect()).collect()
    }

    #[test]
    fn columns_line_up_across_rows() {
        assert_eq!(rendered(&["name,age,city", "alice,30,NYC", "bo,7,LA"], csv()), vec!["name,  age, city", "alice, 30,  NYC", "bo,    7,   LA"]);
    }

    // The file's characters come back in order, padding aside.
    #[test]
    fn rendering_only_ever_inserts_spaces() {
        for line in ["a,b,c", "alice,30,NYC", "\"x,y\",z", "", "no delimiter here"] {
            let cs = chars(line);
            let layout = one(&cs, csv());
            let r = row(&cs, 0, &layout);
            let back: String = r.cells.iter().zip(&r.source_at).filter(|(_, s)| s.is_some()).map(|(c, _)| *c).collect();
            assert_eq!(back, line, "the line's own characters must survive exactly");
            assert!(r.cells.iter().zip(&r.source_at).all(|(c, s)| s.is_some() || *c == ' '), "only spaces are added");
        }
    }

    // The two maps agree, or the cursor lands where the text isn't.
    #[test]
    fn the_two_maps_are_inverses() {
        let cs = chars("alice,30,NYC");
        let layout = one(&cs, csv());
        let r = row(&cs, 0, &layout);
        for (i, cell) in r.cell_of.iter().enumerate().take(cs.len()) {
            assert_eq!(r.source_at[*cell], Some(i), "character {i} maps to a cell that maps back to it");
        }
        assert_eq!(r.cell_of[cs.len()], r.cells.len());
    }

    #[test]
    fn a_line_with_no_delimiter_is_left_exactly_as_it_is() {
        let cs = chars("just a sentence");
        let layout = one(&cs, csv());
        assert_eq!(row(&cs, 0, &layout), Row::plain(&cs));
    }

    // By display width, so a CJK field doesn't throw the column off.
    #[test]
    fn alignment_measures_display_width() {
        let out = rendered(&["\u{65e5}\u{672c},x", "ab,y"], csv());
        assert_eq!(out[0], "\u{65e5}\u{672c}, x");
        assert_eq!(out[1], "ab,   y");
    }

    #[test]
    fn a_pathological_file_does_not_produce_a_pathological_layout() {
        let wide = format!("{},b", "x".repeat(500));
        let many = (0..500).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        let all: Vec<Vec<char>> = vec![chars(&wide), chars(&many)];
        let refs: Vec<&[char]> = all.iter().map(|l| l.as_slice()).collect();
        let layout = measure(&refs, csv());
        let widths = &layout.regions[0].widths;
        assert!(widths.iter().all(|w| *w <= MAX_COLUMN_WIDTH));
        assert!(widths.len() <= MAX_COLUMNS);
        // ...and a field wider than its clamped column renders in full.
        let r = row(&all[0], 0, &layout);
        let back: String = r.cells.iter().zip(&r.source_at).filter(|(_, s)| s.is_some()).map(|(c, _)| *c).collect();
        assert_eq!(back, wide);
    }

    fn md() -> Style {
        style("markdown").unwrap()
    }

    #[test]
    fn a_markdown_table_lines_its_pipes_up() {
        assert_eq!(
            rendered_regions(&["| Key | Does |", "|---|:--|", "| gg | goes to the top |", "| G | end |"], md(), &[0..4],),
            vec!["| Key | Does            |", "|---  |:--              |", "| gg  | goes to the top |", "| G   | end             |",]
        );
    }

    // ...and the cell holding the pipe is one cell in the layout too.
    #[test]
    fn an_escaped_pipe_lines_up_as_one_cell() {
        assert_eq!(
            rendered_regions(&[r"| substr 1 echo \| cut | 854 |", "| substr 1 builtin | 358,092 |"], md(), &[0..2]),
            vec![r"| substr 1 echo \| cut | 854     |", "| substr 1 builtin     | 358,092 |"]
        );
    }

    // The separator row is padded, but its dashes don't set the width.
    #[test]
    fn a_rule_row_is_padded_but_does_not_set_the_width() {
        assert_eq!(rendered_regions(&["| a | b |", "|--------|--------|"], md(), &[0..2]), vec!["| a | b |", "|--------|--------|"]);
    }

    #[test]
    fn csv_has_no_rule_rows() {
        assert_eq!(rendered(&["ab,cd", "--,--"], csv()), vec!["ab, cd", "--, --"]);
    }

    // Prose around a table is untouched despite containing the delimiter.
    #[test]
    fn a_line_outside_every_region_is_left_alone() {
        let out = rendered_regions(&["a | b in prose", "| x | yy |", "| zzz | w |"], md(), &[1..3]);
        assert_eq!(out[0], "a | b in prose");
        assert_eq!(out[1], "| x   | yy |");
        assert_eq!(out[2], "| zzz | w  |");
    }

    #[test]
    fn each_region_has_its_own_widths() {
        let out = rendered_regions(&["| a | b |", "", "| aaaaaaaa | b |"], md(), &[0..1, 2..3]);
        assert_eq!(out[0], "| a | b |");
        assert_eq!(out[2], "| aaaaaaaa | b |");
    }

    #[test]
    fn a_region_past_the_end_of_the_file_does_not_panic() {
        assert_eq!(rendered_regions(&["| a | b |"], md(), &[0..99]), vec!["| a | b |"]);
    }
}
