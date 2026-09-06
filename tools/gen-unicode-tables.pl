#!/usr/bin/perl
# Writes src/bishedit/unicode_tables.rs from the Unicode character
# database this machine already has.
#
# Every other table in this project is derived from something that was
# read: a spec, a man page, bash's own behaviour. These two modules --
# unicode_width.rs and grapheme.rs -- were the exception, and said so:
# their ranges were written from memory as a "deliberately trimmed,
# high-confidence subset", which is a polite way of saying nobody could
# check them. That method had already produced one real bug (0xE0100
# sorted after 0x1F3FB, found only because a test asserted the tables
# were sorted).
#
# The data was never the hard part. Perl ships the full UCD and a
# documented API onto it, and has since 5.8 -- so the tables come from
# the system, exactly as the shell's own signal names, terminfo and
# locale data do, rather than from a crate that would vendor the same
# numbers behind a version bound. Python's `unicodedata` carries the
# same database; the East Asian widths are read from both and compared,
# and this script stops if they disagree, so a table can only be wrong
# if two independent copies of the UCD are wrong the same way.
#
# Run it when the tables should follow a newer Unicode, and commit what
# it writes:
#
#     perl tools/gen-unicode-tables.pl > src/bishedit/unicode_tables.rs && cargo fmt
#
# (`cargo fmt` because it puts the short tables back on one line, and a
# file that is not formatted the way the rest of the tree is would show
# up as a diff every time anyone else ran it.)
#
# It is not a build step and must never become one: the output is
# checked in and read like any other source file, so building bish
# needs a Rust compiler and nothing else.

use strict;
use warnings;
use Unicode::UCD qw(prop_invlist prop_invmap);

my $VERSION = Unicode::UCD::UnicodeVersion();

# --- reading the database --------------------------------------------
#
# `prop_invmap` returns an inversion map: a list of range starts, and
# what the property is for each range up to the next start. Turning that
# into "the ranges where the property is one of these values" is the
# only shape this script needs.

# Perl reports Grapheme_Cluster_Break with the Indic-conjunct and
# emoji-pictographic distinctions folded into the value, so `Extend`
# arrives as `InCB_Extend_EX` and `Other` as `ExtPict_XX`. The
# decoration is a prefix and the real value is the abbreviation it ends
# with; a value with no such suffix is already the value itself.
#
# `check` below is what makes this safe to assume: a spelling this does
# not recognise produces an empty table, and an empty table stops the
# script rather than being written out.
my %GCB_ABBREVIATION = (EX => 'Extend', XX => 'Other', CN => 'Control', PP => 'Prepend', SM => 'SpacingMark');

sub gcb_value {
    my $v = shift // '';
    return $GCB_ABBREVIATION{$1} if $v =~ /_([A-Z]{2})$/ && exists $GCB_ABBREVIATION{$1};
    return $v;
}

sub ranges_where {
    my ($starts, $values, $wanted, $normalize) = @_;
    my %want = map { $_ => 1 } @$wanted;
    my @out;
    for my $i (0 .. $#$starts) {
        my $value = $normalize ? $normalize->($values->[$i]) : ($values->[$i] // '');
        next unless $want{$value};
        my $lo = $starts->[$i];
        my $hi = ($i < $#$starts ? $starts->[ $i + 1 ] - 1 : 0x10FFFF);
        push @out, [ $lo, $hi ];
    }
    return merge(@out);
}

# An inversion list is a flat list of alternating start/end-exclusive
# boundaries: the property holds from element 0 up to element 1, from
# element 2 up to element 3, and so on.
sub ranges_from_invlist {
    my @list = @_;
    my @out;
    for (my $i = 0; $i < @list; $i += 2) {
        my $hi = ($i + 1 < @list ? $list[ $i + 1 ] - 1 : 0x10FFFF);
        push @out, [ $list[$i], $hi ];
    }
    return merge(@out);
}

# Adjacent and overlapping ranges become one. Rust's own table is
# binary-searched, so the ranges have to be sorted and disjoint -- and
# fewer of them is a smaller table for free.
sub merge {
    my @in = sort { $a->[0] <=> $b->[0] } @_;
    my @out;
    for my $r (@in) {
        if (@out && $r->[0] <= $out[-1][1] + 1) {
            $out[-1][1] = $r->[1] if $r->[1] > $out[-1][1];
        } else {
            push @out, [ @$r ];
        }
    }
    return @out;
}

sub subtract {
    my ($ranges, @remove) = @_;
    my %gone = map { $_ => 1 } @remove;
    my @out;
    for my $r (@$ranges) {
        my $start = $r->[0];
        for my $cp ($r->[0] .. $r->[1]) {
            next unless $gone{$cp};
            push @out, [ $start, $cp - 1 ] if $cp > $start;
            $start = $cp + 1;
        }
        push @out, [ $start, $r->[1] ] if $start <= $r->[1];
    }
    return @out;
}

my ($ea_starts,  $ea_values)  = prop_invmap('East_Asian_Width');
my ($gcb_starts, $gcb_values) = prop_invmap('Grapheme_Cluster_Break');

my @wide = ranges_where($ea_starts, $ea_values, [ 'W', 'F' ]);
my @extend = ranges_where($gcb_starts, $gcb_values, ['Extend'], \&gcb_value);
my @control = ranges_where($gcb_starts, $gcb_values, ['Control'], \&gcb_value);
my @prepend = ranges_where($gcb_starts, $gcb_values, ['Prepend'], \&gcb_value);
my @spacing_mark = ranges_where($gcb_starts, $gcb_values, ['SpacingMark'], \&gcb_value);
my @hangul_l = ranges_where($gcb_starts, $gcb_values, ['L'], \&gcb_value);
my @hangul_v = ranges_where($gcb_starts, $gcb_values, ['V'], \&gcb_value);
my @hangul_t = ranges_where($gcb_starts, $gcb_values, ['T'], \&gcb_value);
my @pictographic = ranges_from_invlist(prop_invlist('Extended_Pictographic'));

# What occupies no column. Grapheme_Extend is the combining marks and
# everything that behaves like one; the format characters (Cf) are the
# joiners, direction marks and the byte-order mark, which a terminal
# draws nothing for; and Hangul V and T are the medial and final jamo,
# which compose into the syllable the leading jamo already started.
#
# U+00AD, the soft hyphen, is the one exception carved out: it is a Cf
# but every terminal gives it a column, and so does wcwidth.
my ($gc_starts, $gc_values) = prop_invmap('General_Category');
my @cf = ranges_where($gc_starts, $gc_values, ['Cf']);
my @grapheme_extend = ranges_from_invlist(prop_invlist('Grapheme_Extend'));
my @zero_width = merge(@grapheme_extend, @cf, @hangul_v, @hangul_t);
@zero_width = subtract(\@zero_width, 0x00AD);

# --- does what came back look like the Unicode everyone else has? -----
#
# A property spelled differently by a future Perl would otherwise
# produce an empty table and no complaint at all -- which is how the
# first run of this script quietly emitted three ranges for Extend
# instead of four hundred. Every table is checked against a character
# whose membership is not in question.

sub check {
    my ($name, $ranges, $cp, $description) = @_;
    die "$name came back empty -- the property is not spelled the way this script expects\n" unless @$ranges;
    for my $r (@$ranges) {
        return if $r->[0] <= $cp && $cp <= $r->[1];
    }
    die sprintf("%s does not contain U+%04X (%s), so it is not the property it claims to be\n", $name, $cp, $description);
}

check('WIDE',                  \@wide,          0x4E00,  'a CJK ideograph');
check('WIDE',                  \@wide,          0xFF21,  'a fullwidth A');
check('ZERO_WIDTH',            \@zero_width,    0x0301,  'combining acute accent');
check('ZERO_WIDTH',            \@zero_width,    0xFE0F,  'variation selector 16');
check('ZERO_WIDTH',            \@zero_width,    0x200D,  'zero-width joiner');
check('EXTEND',                \@extend,        0x0301,  'combining acute accent');
check('EXTEND',                \@extend,        0x1F3FB, 'an emoji skin-tone modifier');
check('CONTROL',               \@control,       0x0007,  'the bell');
check('PREPEND',               \@prepend,       0x0600,  'arabic number sign');
check('SPACING_MARK',          \@spacing_mark,  0x0903,  'devanagari sign visarga');
check('HANGUL_L',              \@hangul_l,      0x1100,  'hangul choseong kiyeok');
check('HANGUL_V',              \@hangul_v,      0x1160,  'hangul jungseong filler');
check('HANGUL_T',              \@hangul_t,      0x11A8,  'hangul jongseong kiyeok');
check('EXTENDED_PICTOGRAPHIC', \@pictographic,  0x1F600, 'a grinning face');

# `subtract` is only ever asked to remove the soft hyphen, and a table
# that still has it means the carve-out silently stopped working.
die "U+00AD is still zero width\n" if grep { $_->[0] <= 0x00AD && 0x00AD <= $_->[1] } @zero_width;

# --- cross-check against the other copy of the database ---------------

my $python = qx{python3 -c 'import unicodedata as u; print(u.unidata_version); print(",".join(str(cp) for cp in range(0x110000) if u.east_asian_width(chr(cp)) in ("W","F")))' 2>/dev/null};
if ($python) {
    my ($their_version, $their_wide) = split /\n/, $python, 2;
    chomp $their_version;
    chomp $their_wide;
    die "perl has UCD $VERSION and python has UCD $their_version -- the two disagree about which Unicode this is\n"
      unless $their_version eq $VERSION;
    my %mine;
    for my $r (@wide) { $mine{$_} = 1 for $r->[0] .. $r->[1] }
    my @theirs = split /,/, $their_wide;
    my %theirs = map { $_ => 1 } @theirs;
    for my $cp (keys %mine) {
        die sprintf("perl says U+%04X is wide and python does not\n", $cp) unless $theirs{$cp};
    }
    for my $cp (@theirs) {
        die sprintf("python says U+%04X is wide and perl does not\n", $cp) unless $mine{$cp};
    }
} else {
    warn "no python3 to cross-check the widths against -- one copy of the UCD is all this ran on\n";
}

# --- writing the table ------------------------------------------------

sub table {
    my ($name, $what, @ranges) = @_;
    my $out = "/// $what\npub const $name: &[(u32, u32)] = &[\n";
    for my $r (@ranges) {
        $out .= sprintf("    (0x%04X, 0x%04X),\n", $r->[0], $r->[1]);
    }
    return $out . "];\n\n";
}

print <<"HEADER";
// Generated by tools/gen-unicode-tables.pl from Unicode $VERSION -- do
// not edit. Run that script and commit its output instead; it explains
// where the data comes from and why it is not a build step.
//
// Every range here is sorted and disjoint, which is what lets the
// lookups in unicode_width.rs and grapheme.rs binary-search them. A
// test in unicode_width.rs checks that, because the tables these
// replaced were written from memory and being out of order was a real
// bug rather than a hypothetical one.

HEADER

print "/// The Unicode this was generated from.\npub const UNICODE_VERSION: &str = \"$VERSION\";\n\n";

print table('WIDE', 'East_Asian_Width is Wide or Fullwidth: two columns on a terminal.', @wide);
print table('ZERO_WIDTH', 'No column of its own: combining marks, format characters, and the jamo that compose into a preceding syllable.', @zero_width);
print table('EXTEND', 'Grapheme_Cluster_Break=Extend: joins the cluster before it (UAX #29 GB9).', @extend);
print table('CONTROL', 'Grapheme_Cluster_Break=Control: breaks on both sides (GB4/GB5).', @control);
print table('PREPEND', 'Grapheme_Cluster_Break=Prepend: joins the cluster after it (GB9b).', @prepend);
print table('SPACING_MARK', 'Grapheme_Cluster_Break=SpacingMark: a combining mark that does take a column (GB9a).', @spacing_mark);
print table('HANGUL_L', 'Grapheme_Cluster_Break=L: leading jamo.', @hangul_l);
print table('HANGUL_V', 'Grapheme_Cluster_Break=V: medial jamo.', @hangul_v);
print table('HANGUL_T', 'Grapheme_Cluster_Break=T: trailing jamo.', @hangul_t);
print table('EXTENDED_PICTOGRAPHIC', 'Extended_Pictographic: what a ZWJ may join into one emoji (GB11).', @pictographic);
