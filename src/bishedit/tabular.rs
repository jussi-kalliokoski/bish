// Tabular display for delimiter-separated files: the columns of a CSV
// line up on screen without a single character of the file changing.
//
// **Purely cosmetic, and that is the whole design.** The buffer holds
// exactly what is on disk -- the commas are still there, still yours to
// edit, still what gets saved. All this does is decide where padding
// goes *between* the characters when a row is drawn, so field three of
// one row starts in the same screen column as field three of the next.
// Nothing here can change the text, because nothing here is given a
// mutable buffer.
//
// The consequence to keep in mind while reading the editor side: a
// screen column and a character index stop being the same number, the
// way they already differ for a wide glyph. Every place that converts
// between them goes through the maps this module builds.

use super::unicode_width::char_width;

// How a language's tabular form is shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub delimiter: char,
    pub kind: Kind,
}

// What the delimiter *is* to the row, which is what decides where
// padding may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    // `a,b` -- the delimiter ends the field before it and belongs to
    // it. Padding goes after the delimiter, so the next field starts on
    // the column, and one further space keeps it off the delimiter,
    // since a row like this carries no space of its own.
    Terminated,
    // `| a | b |` -- the delimiters frame the cells between them.
    // Padding goes before the delimiter instead, so it is the frame
    // that lines up, and no extra gap is wanted because the cells
    // already carry their own spaces.
    Framed,
}

impl Kind {
    // Columns are separated by at least this much beyond the widest
    // field.
    fn gap(self) -> usize {
        match self {
            Kind::Terminated => 1,
            Kind::Framed => 0,
        }
    }

    // Whether a row of nothing but rule characters is exempt from
    // deciding column widths. Markdown's `|:---|---:|` is such a row:
    // this can only ever *insert spaces*, never stretch the dashes to
    // fill a column, so letting a long dash run widen the column would
    // buy nothing and cost the whole table its width. A row of dashes
    // in a CSV, by contrast, is just data.
    fn rule_rows(self) -> bool {
        matches!(self, Kind::Framed)
    }
}

// Which style a language's tabular form uses, or `None` for a language
// that has no tabular form at all -- which is most of them, and is why
// the `tabular` bishopt can default to matching everything: a language
// nothing here knows about is simply left alone.
pub fn style(language: &str) -> Option<Style> {
    match language {
        "csv" => Some(Style { delimiter: ',', kind: Kind::Terminated }),
        // A real `.tsv` holds literal tabs, which this editor's
        // one-character-per-column rendering can't place correctly in
        // the first place (see run_insert_mode's own Tab handling) --
        // alignment here is only as good as that already is.
        "tsv" | "tab" => Some(Style { delimiter: '\t', kind: Kind::Terminated }),
        "psv" => Some(Style { delimiter: '|', kind: Kind::Terminated }),
        "markdown" => Some(Style { delimiter: '|', kind: Kind::Framed }),
        _ => None,
    }
}

// A row of nothing but rule characters -- markdown's `|:---|---:|` and
// nothing else. Deliberately whole-line: a *field* of dashes inside an
// ordinary row is content, and only a row that is entirely rule is the
// table's own separator.
fn is_rule_row(line: &[char]) -> bool {
    !line.is_empty() && line.iter().any(|c| *c == '-') && line.iter().all(|c| matches!(c, '-' | ':' | '|' | ' ' | '\t'))
}

// A pathological file must not produce a pathological layout: a column
// wider than this is truncated for *alignment* purposes (its own text is
// never cut, the row simply runs long), and columns past this many are
// left unaligned.
const MAX_COLUMN_WIDTH: usize = 60;
const MAX_COLUMNS: usize = 128;

// One independently aligned run of lines, and the column widths it
// agreed on. A CSV is a single region covering the file; a markdown
// document is one per table, because two tables in one file have
// nothing to do with each other and aligning them together would make
// each as wide as the other needs.
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

// The whole file as one aligned region -- what a CSV wants.
pub fn measure(lines: &[&[char]], style: Style) -> Layout {
    measure_regions(lines, style, &[0..lines.len()])
}

// The widest field in each column, per region. Measured across every
// line of the region rather than the visible window, so a column doesn't
// change width as you scroll; a line in no region is left alone.
pub fn measure_regions(lines: &[&[char]], style: Style, regions: &[std::ops::Range<usize>]) -> Layout {
    let mut out = Vec::new();
    for region in regions {
        let mut widths: Vec<usize> = Vec::new();
        for line in lines[region.start.min(lines.len())..region.end.min(lines.len())].iter() {
            if style.kind.rule_rows() && is_rule_row(line) {
                continue;
            }
            for (i, field) in fields(line, style.delimiter).into_iter().enumerate() {
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

// One line's fields, as character ranges. RFC 4180 quoting is honoured:
// a field that opens with `"` runs to its closing quote, `""` inside it
// is an escaped quote, and a delimiter inside quotes does not split.
// That matters for display exactly as much as it matters for parsing --
// a quoted address containing a comma is one column, and aligning it as
// two would misrepresent the file.
pub fn fields(line: &[char], delimiter: char) -> Vec<Field> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut quoted = false;
    while i < line.len() {
        let c = line[i];
        if quoted {
            if c == '"' {
                // A doubled quote is an escaped one and stays inside.
                if line.get(i + 1) == Some(&'"') {
                    i += 2;
                    continue;
                }
                quoted = false;
            }
            i += 1;
            continue;
        }
        if c == '"' && i == start {
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
    // What to draw -- the line's own characters, with padding spliced in.
    pub cells: Vec<char>,
    // For each cell, which character of the line it came from. `None`
    // for a padding cell, which belongs to no character at all.
    pub source_at: Vec<Option<usize>>,
    // For each character of the line, the cell it starts at. One longer
    // than the line, so the position *after* the last character (where
    // the cursor sits at end of line) has an answer too.
    pub cell_of: Vec<usize>,
}

impl Row {
    // The identity row: every character is its own cell, nothing added.
    // What a language with no tabular form gets, and what makes the
    // editor able to run one code path for both cases.
    pub fn plain(line: &[char]) -> Row {
        Row { cells: line.to_vec(), source_at: (0..line.len()).map(Some).collect(), cell_of: (0..=line.len()).collect() }
    }
}

// `line`, with padding inserted after each delimiter so that every
// column starts where the layout says it should.
//
// The padding goes *after* the delimiter rather than before it, so a
// comma stays visually attached to the field it terminates and each
// field starts at a predictable column. A field wider than its column's
// measured width (only possible once MAX_COLUMN_WIDTH has clamped that
// width) simply pushes the rest of its row along rather than being cut.
pub fn row(line: &[char], line_number: usize, layout: &Layout) -> Row {
    // A line outside every region -- prose around a markdown table, or
    // anything at all in a file whose language has no tabular form -- is
    // drawn exactly as it is.
    let Some(widths) = layout.widths_for(line_number) else { return Row::plain(line) };
    let fields = fields(line, layout.style.delimiter);
    let mut cells = Vec::with_capacity(line.len());
    let mut source_at: Vec<Option<usize>> = Vec::with_capacity(line.len());
    let mut cell_of = vec![0usize; line.len() + 1];
    // A line with no delimiter at all -- a comment, a blank line, a
    // trailing newline's empty last row -- is not a row of a table and
    // is left exactly as it is.
    if fields.len() <= 1 {
        return Row::plain(line);
    }
    for (index, field) in fields.iter().enumerate() {
        for i in field.start..field.end {
            cell_of[i] = cells.len();
            cells.push(line[i]);
            source_at.push(Some(i));
        }
        // The delimiter, and the padding that squares the column up --
        // in whichever order this style puts them. The last field has
        // neither, since there is no delimiter after it and nothing
        // beyond it to line up with.
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

    fn split(line: &str, delimiter: char) -> Vec<String> {
        let cs = chars(line);
        fields(&cs, delimiter).iter().map(|f| cs[f.start..f.end].iter().collect()).collect()
    }

    #[test]
    fn a_language_without_a_tabular_form_has_no_style() {
        assert_eq!(style("csv").map(|s| s.delimiter), Some(','));
        assert_eq!(style("tsv").map(|s| s.delimiter), Some('\t'));
        assert_eq!(style("markdown").map(|s| s.delimiter), Some('|'));
        assert_eq!(style("rust"), None);
    }

    // The two differ in more than the delimiter: a CSV row carries no
    // space of its own, a markdown row already carries its own.
    #[test]
    fn csv_terminates_its_fields_and_markdown_frames_them() {
        assert_eq!(style("csv").unwrap().kind, Kind::Terminated);
        assert_eq!(style("markdown").unwrap().kind, Kind::Framed);
    }

    #[test]
    fn fields_split_on_the_delimiter() {
        assert_eq!(split("a,b,c", ','), vec!["a", "b", "c"]);
        assert_eq!(split("a,,c", ','), vec!["a", "", "c"]);
        assert_eq!(split("", ','), vec![""]);
        assert_eq!(split("solo", ','), vec!["solo"]);
    }

    // A quoted field holding a delimiter is one column. Aligning it as
    // two would misrepresent the file.
    #[test]
    fn a_delimiter_inside_quotes_does_not_split() {
        assert_eq!(split("a,\"b,c\",d", ','), vec!["a", "\"b,c\"", "d"]);
        assert_eq!(split("\"one, two\"", ','), vec!["\"one, two\""]);
    }

    #[test]
    fn a_doubled_quote_is_an_escaped_quote_and_stays_inside() {
        assert_eq!(split("\"say \"\"hi\"\", ok\",next", ','), vec!["\"say \"\"hi\"\", ok\"", "next"]);
    }

    // A quote that isn't at the start of a field is just a character.
    #[test]
    fn a_quote_mid_field_does_not_open_a_quoted_field() {
        assert_eq!(split("a\"b,c", ','), vec!["a\"b", "c"]);
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

    // Nothing is removed and nothing is reordered: the file's own
    // characters come back in order, padding aside.
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

    // The two maps have to agree, or the cursor lands somewhere the text
    // isn't.
    #[test]
    fn the_two_maps_are_inverses() {
        let cs = chars("alice,30,NYC");
        let layout = one(&cs, csv());
        let r = row(&cs, 0, &layout);
        for (i, cell) in r.cell_of.iter().enumerate().take(cs.len()) {
            assert_eq!(r.source_at[*cell], Some(i), "character {i} maps to a cell that maps back to it");
        }
        // And the position after the last character is one past the end.
        assert_eq!(r.cell_of[cs.len()], r.cells.len());
    }

    #[test]
    fn a_line_with_no_delimiter_is_left_exactly_as_it_is() {
        let cs = chars("just a sentence");
        let layout = one(&cs, csv());
        assert_eq!(row(&cs, 0, &layout), Row::plain(&cs));
    }

    // Alignment is by display width, so a CJK field doesn't throw the
    // column off.
    #[test]
    fn alignment_measures_display_width() {
        let out = rendered(&["\u{65e5}\u{672c},x", "ab,y"], csv());
        // The CJK field draws 4 columns, so `ab` is padded to match.
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
        // ...and a field wider than its clamped column still renders in
        // full rather than being cut.
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

    // The separator row is padded like any other, but a long dash run in
    // it must not decide the column's width -- this can only insert
    // spaces, never stretch the dashes to fill what it asked for.
    #[test]
    fn a_rule_row_is_padded_but_does_not_set_the_width() {
        assert_eq!(rendered_regions(&["| a | b |", "|--------|--------|"], md(), &[0..2]), vec!["| a | b |", "|--------|--------|"]);
    }

    // A row of dashes in a CSV is data, not a rule.
    #[test]
    fn csv_has_no_rule_rows() {
        assert_eq!(rendered(&["ab,cd", "--,--"], csv()), vec!["ab, cd", "--, --"]);
    }

    // Prose around a table is outside every region, so it comes back
    // untouched even though it contains the delimiter.
    #[test]
    fn a_line_outside_every_region_is_left_alone() {
        let out = rendered_regions(&["a | b in prose", "| x | yy |", "| zzz | w |"], md(), &[1..3]);
        assert_eq!(out[0], "a | b in prose");
        assert_eq!(out[1], "| x   | yy |");
        assert_eq!(out[2], "| zzz | w  |");
    }

    // Two tables in one document are unrelated: a wide column in one
    // must not widen the other.
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
