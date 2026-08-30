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

// Which delimiter a language's tabular form uses, or `None` for a
// language that has no tabular form at all -- which is most of them, and
// is why the `tabular` bishopt can default to matching everything: a
// language nothing here knows about is simply left alone.
pub fn delimiter(language: &str) -> Option<char> {
    match language {
        "csv" => Some(','),
        "tsv" | "tab" => Some('\t'),
        "psv" => Some('|'),
        _ => None,
    }
}

// A pathological file must not produce a pathological layout: a column
// wider than this is truncated for *alignment* purposes (its own text is
// never cut, the row simply runs long), and columns past this many are
// left unaligned.
const MAX_COLUMN_WIDTH: usize = 60;
const MAX_COLUMNS: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    delimiter: char,
    // Display width of each column's widest field.
    widths: Vec<usize>,
}

impl Layout {
    pub fn columns(&self) -> usize {
        self.widths.len()
    }

    pub fn delimiter(&self) -> char {
        self.delimiter
    }
}

// The widest field in each column, across every line -- computed over
// the whole file rather than the visible window, so a column doesn't
// change width as you scroll.
pub fn measure<'a>(lines: impl Iterator<Item = &'a [char]>, delimiter: char) -> Layout {
    let mut widths: Vec<usize> = Vec::new();
    for line in lines {
        for (i, field) in fields(line, delimiter).into_iter().enumerate() {
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
    Layout { delimiter, widths }
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
        Row {
            cells: line.to_vec(),
            source_at: (0..line.len()).map(Some).collect(),
            cell_of: (0..=line.len()).collect(),
        }
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
pub fn row(line: &[char], layout: &Layout) -> Row {
    let fields = fields(line, layout.delimiter);
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
        // The delimiter itself, then the padding that squares the column
        // up. The last field has neither.
        if field.end < line.len() {
            cell_of[field.end] = cells.len();
            cells.push(line[field.end]);
            source_at.push(Some(field.end));
            let width: usize = line[field.start..field.end].iter().map(|c| char_width(*c)).sum();
            let target = layout.widths.get(index).copied().unwrap_or(width);
            // One space of gap beyond the widest field, so columns never
            // touch.
            let pad = target.saturating_sub(width) + 1;
            for _ in 0..pad {
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
    fn a_language_without_a_tabular_form_has_no_delimiter() {
        assert_eq!(delimiter("csv"), Some(','));
        assert_eq!(delimiter("tsv"), Some('\t'));
        assert_eq!(delimiter("rust"), None);
        assert_eq!(delimiter("markdown"), None);
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

    fn rendered(lines: &[&str], delimiter: char) -> Vec<String> {
        let all: Vec<Vec<char>> = lines.iter().map(|l| chars(l)).collect();
        let layout = measure(all.iter().map(|l| l.as_slice()), delimiter);
        all.iter().map(|l| row(l, &layout).cells.iter().collect()).collect()
    }

    #[test]
    fn columns_line_up_across_rows() {
        assert_eq!(
            rendered(&["name,age,city", "alice,30,NYC", "bo,7,LA"], ','),
            vec!["name,  age, city", "alice, 30,  NYC", "bo,    7,   LA"]
        );
    }

    // Nothing is removed and nothing is reordered: the file's own
    // characters come back in order, padding aside.
    #[test]
    fn rendering_only_ever_inserts_spaces() {
        for line in ["a,b,c", "alice,30,NYC", "\"x,y\",z", "", "no delimiter here"] {
            let cs = chars(line);
            let layout = measure(std::iter::once(cs.as_slice()), ',');
            let r = row(&cs, &layout);
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
        let layout = measure(std::iter::once(cs.as_slice()), ',');
        let r = row(&cs, &layout);
        for (i, cell) in r.cell_of.iter().enumerate().take(cs.len()) {
            assert_eq!(r.source_at[*cell], Some(i), "character {i} maps to a cell that maps back to it");
        }
        // And the position after the last character is one past the end.
        assert_eq!(r.cell_of[cs.len()], r.cells.len());
    }

    #[test]
    fn a_line_with_no_delimiter_is_left_exactly_as_it_is() {
        let cs = chars("just a sentence");
        let layout = measure(std::iter::once(cs.as_slice()), ',');
        assert_eq!(row(&cs, &layout), Row::plain(&cs));
    }

    // Alignment is by display width, so a CJK field doesn't throw the
    // column off.
    #[test]
    fn alignment_measures_display_width() {
        let out = rendered(&["\u{65e5}\u{672c},x", "ab,y"], ',');
        // The CJK field draws 4 columns, so `ab` is padded to match.
        assert_eq!(out[0], "\u{65e5}\u{672c}, x");
        assert_eq!(out[1], "ab,   y");
    }

    #[test]
    fn a_pathological_file_does_not_produce_a_pathological_layout() {
        let wide = format!("{},b", "x".repeat(500));
        let many = (0..500).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        let all: Vec<Vec<char>> = vec![chars(&wide), chars(&many)];
        let layout = measure(all.iter().map(|l| l.as_slice()), ',');
        assert!(layout.widths.iter().all(|w| *w <= MAX_COLUMN_WIDTH));
        assert!(layout.columns() <= MAX_COLUMNS);
        // ...and a field wider than its clamped column still renders in
        // full rather than being cut.
        let r = row(&all[0], &layout);
        let back: String = r.cells.iter().zip(&r.source_at).filter(|(_, s)| s.is_some()).map(|(c, _)| *c).collect();
        assert_eq!(back, wide);
    }
}
