// Hand-rolled CSS Color parser -- no external crate, same spirit as
// glob.rs/regex.rs. Supports: named colors (the standard 148 CSS Color
// keywords, plus `transparent`), #hex (3/4/6/8 digit), rgb()/rgba(),
// hsl()/hsla(), hwb(), and color-mix() -- the "color math" piece,
// restricted to the three interpolation spaces this hand-rolled parser
// can mix correctly by just averaging channels (srgb, hsl, hwb).
// Perceptual spaces (lab()/lch()/oklab()/oklch(), color(display-p3 ...)),
// relative color syntax (`rgb(from ... )`), calc() inside individual
// channels, and light-dark() are all out of scope -- those need real
// colorimetry (matrix transforms, calibrated primaries) or surrounding
// context this parser doesn't have, not just more parsing.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Rgba {
        Rgba { r, g, b, a }
    }

    // Canonical text form: 6-digit hex when fully opaque (the common
    // case, and what most people mean by "a color"), 8-digit otherwise --
    // never 3/4-digit shorthand, so this is always round-trippable
    // through `parse` without loss. #[allow(dead_code)]: bishopt's own
    // `get` deliberately echoes back the *original* text a color was set
    // to rather than calling this (see exec.rs's BishOptValue::Color),
    // but a resolved Rgba still needs a canonical serialization for
    // whatever eventually actually *renders* a color (a prompt segment,
    // truecolor escape codes, ...) -- this is that, kept ready for it.
    #[allow(dead_code)]
    pub fn to_hex(self) -> String {
        if self.a == 255 {
            format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            format!("#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }
}

pub fn parse(input: &str) -> Result<Rgba, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty color".to_string());
    }
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex(hex);
    }
    if let Some(open) = s.find('(') {
        if !s.ends_with(')') {
            return Err(format!("{s}: unterminated color function"));
        }
        let name = s[..open].trim().to_ascii_lowercase();
        let inner = &s[open + 1..s.len() - 1];
        return match name.as_str() {
            "rgb" | "rgba" => parse_rgb(inner),
            "hsl" | "hsla" => parse_hsl(inner),
            "hwb" => parse_hwb(inner),
            "color-mix" => parse_color_mix(inner),
            _ => Err(format!("{name}(): unsupported color function")),
        };
    }
    let lower = s.to_ascii_lowercase();
    if lower == "transparent" {
        return Ok(Rgba::new(0, 0, 0, 0));
    }
    named_color(&lower).ok_or_else(|| format!("{s}: not a valid CSS color"))
}

// A resolved color that's either a concrete Rgba (anything `parse` above
// understands) or a still-symbolic reference into the *terminal's own*
// palette (an xterm-indexed slot 0-255, of which 0-15 are typically
// user-themed) -- bish has no fixed RGB for that, only the terminal
// displaying it does, so it can't be folded into Rgba the way every
// other color form here is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TermColor {
    Rgba(Rgba),
    Ansi(u8),
}

// Vendor-prefixed extension, CSS's own convention for "not a standard
// property/value, but shaped like one" (`-webkit-...`, `-moz-...`) --
// `-bish-<name>` for one of the 16 standard ANSI slots by name, or
// `-bish-ansi(<0-255>)` for any xterm-indexed slot by number (the 16
// named ones are just `-bish-ansi(0)`..`-bish-ansi(15)` with a memorable
// spelling). Deliberately its own entry point, not folded into `parse`
// above: every other color form here resolves to a concrete Rgba that
// color-mix() etc. can do real math on, but a terminal-palette color has
// no fixed RGB bish itself knows -- `color-mix(in srgb, -bish-red,
// blue)` has nothing to mix *with*, so it's simply not a color `parse`
// (what color-mix's own component parsing calls) recognizes at all;
// only a bare top-level value can be a terminal color.
pub fn parse_terminal(input: &str) -> Result<TermColor, String> {
    let s = input.trim();
    match s.strip_prefix("-bish-") {
        Some(rest) => parse_vendor(rest),
        None => parse(s).map(TermColor::Rgba),
    }
}

fn parse_vendor(rest: &str) -> Result<TermColor, String> {
    if let Some(open) = rest.find('(') {
        if rest[..open].trim() != "ansi" || !rest.ends_with(')') {
            return Err(format!("-bish-{rest}: unsupported vendor color function"));
        }
        let inner = rest[open + 1..rest.len() - 1].trim();
        let n: u16 = inner.parse().map_err(|_| format!("-bish-ansi({inner}): expected an integer 0-255"))?;
        return u8::try_from(n).map(TermColor::Ansi).map_err(|_| format!("-bish-ansi({n}): out of range, must be 0-255"));
    }
    ANSI_NAMES.iter().find(|(n, _)| *n == rest).map(|(_, idx)| TermColor::Ansi(*idx)).ok_or_else(|| format!("-bish-{rest}: not a known terminal palette color"))
}

// The 16 standard ANSI slots' conventional names -- the same 8 base +
// "bright" variant vocabulary terminal emulators themselves use in their
// own palette settings (iTerm2/kitty/Alacritty/...), so `-bish-red`
// means exactly the same slot a user's own terminal config already calls
// "red".
const ANSI_NAMES: &[(&str, u8)] = &[
    ("black", 0),
    ("red", 1),
    ("green", 2),
    ("yellow", 3),
    ("blue", 4),
    ("magenta", 5),
    ("cyan", 6),
    ("white", 7),
    ("bright-black", 8),
    ("bright-red", 9),
    ("bright-green", 10),
    ("bright-yellow", 11),
    ("bright-blue", 12),
    ("bright-magenta", 13),
    ("bright-cyan", 14),
    ("bright-white", 15),
];

fn named_color(name: &str) -> Option<Rgba> {
    NAMED_COLORS.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

fn parse_hex(hex: &str) -> Result<Rgba, String> {
    let chars: Vec<char> = hex.chars().collect();
    let nibble = |c: char| -> Result<u8, String> { c.to_digit(16).map(|d| d as u8).ok_or_else(|| format!("#{hex}: '{c}' isn't a hex digit")) };
    let short = |c: char| -> Result<u8, String> {
        let n = nibble(c)?;
        Ok(n * 16 + n)
    };
    let byte = |hi: char, lo: char| -> Result<u8, String> { Ok(nibble(hi)? * 16 + nibble(lo)?) };
    match chars.len() {
        3 => Ok(Rgba::new(short(chars[0])?, short(chars[1])?, short(chars[2])?, 255)),
        4 => Ok(Rgba::new(short(chars[0])?, short(chars[1])?, short(chars[2])?, short(chars[3])?)),
        6 => Ok(Rgba::new(byte(chars[0], chars[1])?, byte(chars[2], chars[3])?, byte(chars[4], chars[5])?, 255)),
        8 => Ok(Rgba::new(byte(chars[0], chars[1])?, byte(chars[2], chars[3])?, byte(chars[4], chars[5])?, byte(chars[6], chars[7])?)),
        n => Err(format!("#{hex}: hex colors need 3, 4, 6, or 8 digits, got {n}")),
    }
}

fn clamp_u8(v: f64) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

fn frac_to_u8(v: f64) -> u8 {
    clamp_u8(v * 255.0)
}

// Splits an `rgb()`/`hsl()`/`hwb()` function's inner text into its three
// channels plus an optional alpha, honoring both legacy comma syntax
// (`255, 0, 0, 0.5`) and modern space syntax (`255 0 0 / 0.5`).
fn split_channels(inner: &str) -> (Vec<String>, Option<String>) {
    let inner = inner.trim();
    if inner.contains(',') {
        let mut parts: Vec<String> = inner.split(',').map(|p| p.trim().to_string()).collect();
        if parts.len() == 4 {
            let alpha = parts.pop();
            (parts, alpha)
        } else {
            (parts, None)
        }
    } else {
        let (main, alpha) = match inner.split_once('/') {
            Some((m, a)) => (m, Some(a.trim().to_string())),
            None => (inner, None),
        };
        (main.split_whitespace().map(|s| s.to_string()).collect(), alpha)
    }
}

fn parse_alpha(s: &str) -> Result<u8, String> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        let v: f64 = pct.trim().parse().map_err(|_| format!("'{s}' isn't a valid alpha"))?;
        Ok(frac_to_u8(v / 100.0))
    } else {
        let v: f64 = s.parse().map_err(|_| format!("'{s}' isn't a valid alpha"))?;
        Ok(frac_to_u8(v))
    }
}

fn parse_rgb(inner: &str) -> Result<Rgba, String> {
    let (channels, alpha) = split_channels(inner);
    let [c0, c1, c2] = channels.as_slice() else {
        return Err(format!("rgb(): expected 3 channels, got {}", channels.len()));
    };
    let chan = |s: &str| -> Result<u8, String> {
        let s = s.trim();
        if let Some(pct) = s.strip_suffix('%') {
            let v: f64 = pct.trim().parse().map_err(|_| format!("'{s}' isn't a valid rgb() channel"))?;
            Ok(clamp_u8(v / 100.0 * 255.0))
        } else {
            let v: f64 = s.parse().map_err(|_| format!("'{s}' isn't a valid rgb() channel"))?;
            Ok(clamp_u8(v))
        }
    };
    let a = alpha.as_deref().map(parse_alpha).transpose()?.unwrap_or(255);
    Ok(Rgba::new(chan(c0)?, chan(c1)?, chan(c2)?, a))
}

// Leading number, trailing unit -- e.g. "180deg" -> ("180", "deg"). Good
// enough for hue's small unit vocabulary; doesn't need to handle
// scientific notation ("1e2"), which no CSS hue value in practice uses.
fn split_number_unit(s: &str) -> (&str, &str) {
    let idx = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    (&s[..idx], &s[idx..])
}

fn parse_hue(s: &str) -> Result<f64, String> {
    let s = s.trim();
    let (num, unit) = split_number_unit(s);
    let v: f64 = num.parse().map_err(|_| format!("'{s}' isn't a valid hue"))?;
    let deg = match unit {
        "" | "deg" => v,
        "grad" => v * 0.9,
        "rad" => v.to_degrees(),
        "turn" => v * 360.0,
        other => return Err(format!("'{other}' isn't a hue unit bish understands")),
    };
    Ok(deg.rem_euclid(360.0))
}

// hsl()'s saturation/lightness and hwb()'s whiteness/blackness are always
// percentages in CSS -- unlike rgb()'s channels, a bare number is never
// valid here.
fn parse_percent(s: &str) -> Result<f64, String> {
    let s = s.trim();
    let pct = s.strip_suffix('%').ok_or_else(|| format!("'{s}': expected a percentage"))?;
    let v: f64 = pct.trim().parse().map_err(|_| format!("'{s}' isn't a valid percentage"))?;
    Ok((v / 100.0).clamp(0.0, 1.0))
}

fn parse_hsl(inner: &str) -> Result<Rgba, String> {
    let (channels, alpha) = split_channels(inner);
    let [c0, c1, c2] = channels.as_slice() else {
        return Err(format!("hsl(): expected 3 channels, got {}", channels.len()));
    };
    let (r, g, b) = hsl_to_rgb(parse_hue(c0)?, parse_percent(c1)?, parse_percent(c2)?);
    let a = alpha.as_deref().map(parse_alpha).transpose()?.unwrap_or(255);
    Ok(Rgba::new(frac_to_u8(r), frac_to_u8(g), frac_to_u8(b), a))
}

fn parse_hwb(inner: &str) -> Result<Rgba, String> {
    let (channels, alpha) = split_channels(inner);
    let [c0, c1, c2] = channels.as_slice() else {
        return Err(format!("hwb(): expected 3 channels, got {}", channels.len()));
    };
    let (r, g, b) = hwb_to_rgb(parse_hue(c0)?, parse_percent(c1)?, parse_percent(c2)?);
    let a = alpha.as_deref().map(parse_alpha).transpose()?.unwrap_or(255);
    Ok(Rgba::new(frac_to_u8(r), frac_to_u8(g), frac_to_u8(b), a))
}

// Standard HSL->RGB conversion; h in degrees (any range, wraps), s/l in
// 0.0..=1.0. Returns each channel as a 0.0..=1.0 fraction.
fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (f64, f64, f64) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (hp.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    (r1 + m, g1 + m, b1 + m)
}

// hwb() is defined in terms of hsl() with full saturation/half lightness,
// then linearly mixed toward white/black by w/b.
fn hwb_to_rgb(h: f64, w: f64, b: f64) -> (f64, f64, f64) {
    if w + b >= 1.0 {
        let gray = w / (w + b);
        return (gray, gray, gray);
    }
    let (r, g, bl) = hsl_to_rgb(h, 1.0, 0.5);
    let scale = 1.0 - w - b;
    (r * scale + w, g * scale + w, bl * scale + w)
}

fn rgb_to_hsl(c: Rgba) -> (f64, f64, f64) {
    let (r, g, b) = (c.r as f64 / 255.0, c.g as f64 / 255.0, c.b as f64 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d < 1e-9 {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h.rem_euclid(360.0), s, l)
}

fn rgb_to_hwb(c: Rgba) -> (f64, f64, f64) {
    let (h, _, _) = rgb_to_hsl(c);
    let (r, g, b) = (c.r as f64 / 255.0, c.g as f64 / 255.0, c.b as f64 / 255.0);
    (h, r.min(g).min(b), 1.0 - r.max(g).max(b))
}

// Splits on `sep` only at paren-depth 0, so a color-mix() argument that's
// itself a function call (`rgb(1 2 3 / 0.5)`) doesn't get split apart.
fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c == sep && depth == 0 => {
                parts.push(s[start..i].trim());
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(s[start..].trim());
    parts
}

fn split_top_level_ws(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() {
                    parts.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

// A color-mix() argument is `<color>` with an optional percentage, either
// before or after it (both orders are valid CSS).
fn parse_mix_component(s: &str) -> Result<(String, Option<f64>), String> {
    let tokens = split_top_level_ws(s);
    match tokens.as_slice() {
        [color] => Ok((color.clone(), None)),
        [a, b] => {
            if let Some(pct) = a.strip_suffix('%') {
                let v: f64 = pct.parse().map_err(|_| format!("color-mix(): '{a}' isn't a valid percentage"))?;
                Ok((b.clone(), Some(v)))
            } else if let Some(pct) = b.strip_suffix('%') {
                let v: f64 = pct.parse().map_err(|_| format!("color-mix(): '{b}' isn't a valid percentage"))?;
                Ok((a.clone(), Some(v)))
            } else {
                Err(format!("color-mix(): expected a color and a percentage in '{s}'"))
            }
        }
        [] => Err("color-mix(): empty component".to_string()),
        _ => Err(format!("color-mix(): too many tokens in '{s}'")),
    }
}

// Resolves two optional percentages into final 0..=100 mix weights that
// sum to exactly 100, plus an alpha multiplier -- CSS Color 4's own
// color-mix() algorithm: missing percentages default to 50/50 (or fill
// in the complement of whichever one *was* given); if what's given (or
// filled in) doesn't already sum to 100%, both are scaled up
// proportionally so they do, and the result's alpha is scaled down by
// however much they under-summed (mixing 30% of a color with 30% of
// another leaves 40% "nothing", which reads as extra transparency).
fn resolve_mix_weights(p1: Option<f64>, p2: Option<f64>) -> Result<(f64, f64, f64), String> {
    let (mut w1, mut w2) = match (p1, p2) {
        (None, None) => (50.0, 50.0),
        (Some(a), None) => (a, 100.0 - a),
        (None, Some(b)) => (100.0 - b, b),
        (Some(a), Some(b)) => (a, b),
    };
    if w1 < 0.0 || w2 < 0.0 {
        return Err("color-mix(): percentages must not be negative".to_string());
    }
    let sum = w1 + w2;
    if sum <= 0.0 {
        return Err("color-mix(): percentages must not both be zero".to_string());
    }
    let alpha_mult = sum.min(100.0) / 100.0;
    if (sum - 100.0).abs() > 1e-9 {
        w1 = w1 / sum * 100.0;
        w2 = w2 / sum * 100.0;
    }
    Ok((w1, w2, alpha_mult))
}

fn mix_channel(a: u8, b: u8, w1: f64, w2: f64) -> u8 {
    clamp_u8(a as f64 * w1 / 100.0 + b as f64 * w2 / 100.0)
}

fn mix_hue(h1: f64, h2: f64, w2: f64) -> f64 {
    let mut diff = h2 - h1;
    if diff > 180.0 {
        diff -= 360.0;
    } else if diff < -180.0 {
        diff += 360.0;
    }
    (h1 + diff * (w2 / 100.0)).rem_euclid(360.0)
}

fn mix_srgb(c1: Rgba, c2: Rgba, w1: f64, w2: f64) -> Rgba {
    Rgba::new(
        mix_channel(c1.r, c2.r, w1, w2),
        mix_channel(c1.g, c2.g, w1, w2),
        mix_channel(c1.b, c2.b, w1, w2),
        mix_channel(c1.a, c2.a, w1, w2),
    )
}

fn mix_hsl(c1: Rgba, c2: Rgba, w1: f64, w2: f64) -> Rgba {
    let (h1, s1, l1) = rgb_to_hsl(c1);
    let (h2, s2, l2) = rgb_to_hsl(c2);
    let (r, g, b) = hsl_to_rgb(mix_hue(h1, h2, w2), s1 * w1 / 100.0 + s2 * w2 / 100.0, l1 * w1 / 100.0 + l2 * w2 / 100.0);
    Rgba::new(frac_to_u8(r), frac_to_u8(g), frac_to_u8(b), mix_channel(c1.a, c2.a, w1, w2))
}

fn mix_hwb(c1: Rgba, c2: Rgba, w1: f64, w2: f64) -> Rgba {
    let (h1, wh1, bk1) = rgb_to_hwb(c1);
    let (h2, wh2, bk2) = rgb_to_hwb(c2);
    let (r, g, b) = hwb_to_rgb(mix_hue(h1, h2, w2), wh1 * w1 / 100.0 + wh2 * w2 / 100.0, bk1 * w1 / 100.0 + bk2 * w2 / 100.0);
    Rgba::new(frac_to_u8(r), frac_to_u8(g), frac_to_u8(b), mix_channel(c1.a, c2.a, w1, w2))
}

fn scale_alpha(c: Rgba, mult: f64) -> Rgba {
    Rgba::new(c.r, c.g, c.b, clamp_u8(c.a as f64 * mult))
}

fn parse_color_mix(inner: &str) -> Result<Rgba, String> {
    let inner = inner.trim();
    let rest = inner.strip_prefix("in ").ok_or_else(|| format!("color-mix({inner}): expected 'in <space>' first"))?;
    let parts = split_top_level(rest, ',');
    let [space, comp1, comp2] = parts.as_slice() else {
        return Err(format!("color-mix(): expected 'in SPACE, color1, color2', got {} part(s)", parts.len()));
    };
    let (src1, p1) = parse_mix_component(comp1)?;
    let (src2, p2) = parse_mix_component(comp2)?;
    let c1 = parse(&src1)?;
    let c2 = parse(&src2)?;
    let (w1, w2, alpha_mult) = resolve_mix_weights(p1, p2)?;
    let mixed = match space.trim().to_ascii_lowercase().as_str() {
        "srgb" => mix_srgb(c1, c2, w1, w2),
        "hsl" => mix_hsl(c1, c2, w1, w2),
        "hwb" => mix_hwb(c1, c2, w1, w2),
        other => return Err(format!("color-mix(): unsupported interpolation space '{other}' (bish only hand-rolls srgb, hsl, hwb)")),
    };
    Ok(scale_alpha(mixed, alpha_mult))
}

// The 148 standard CSS Color named keywords (the SVG 1.0 list CSS
// adopted verbatim, plus `rebeccapurple`), cross-checked against the
// W3C CSS Color 4 spec's own named-color table. `transparent` isn't
// here -- it's handled directly in `parse` since it's the one keyword
// with alpha 0 rather than 255.
const NAMED_COLORS: &[(&str, Rgba)] = &[
    ("aliceblue", Rgba::new(240, 248, 255, 255)),
    ("antiquewhite", Rgba::new(250, 235, 215, 255)),
    ("aqua", Rgba::new(0, 255, 255, 255)),
    ("aquamarine", Rgba::new(127, 255, 212, 255)),
    ("azure", Rgba::new(240, 255, 255, 255)),
    ("beige", Rgba::new(245, 245, 220, 255)),
    ("bisque", Rgba::new(255, 228, 196, 255)),
    ("black", Rgba::new(0, 0, 0, 255)),
    ("blanchedalmond", Rgba::new(255, 235, 205, 255)),
    ("blue", Rgba::new(0, 0, 255, 255)),
    ("blueviolet", Rgba::new(138, 43, 226, 255)),
    ("brown", Rgba::new(165, 42, 42, 255)),
    ("burlywood", Rgba::new(222, 184, 135, 255)),
    ("cadetblue", Rgba::new(95, 158, 160, 255)),
    ("chartreuse", Rgba::new(127, 255, 0, 255)),
    ("chocolate", Rgba::new(210, 105, 30, 255)),
    ("coral", Rgba::new(255, 127, 80, 255)),
    ("cornflowerblue", Rgba::new(100, 149, 237, 255)),
    ("cornsilk", Rgba::new(255, 248, 220, 255)),
    ("crimson", Rgba::new(220, 20, 60, 255)),
    ("cyan", Rgba::new(0, 255, 255, 255)),
    ("darkblue", Rgba::new(0, 0, 139, 255)),
    ("darkcyan", Rgba::new(0, 139, 139, 255)),
    ("darkgoldenrod", Rgba::new(184, 134, 11, 255)),
    ("darkgray", Rgba::new(169, 169, 169, 255)),
    ("darkgreen", Rgba::new(0, 100, 0, 255)),
    ("darkgrey", Rgba::new(169, 169, 169, 255)),
    ("darkkhaki", Rgba::new(189, 183, 107, 255)),
    ("darkmagenta", Rgba::new(139, 0, 139, 255)),
    ("darkolivegreen", Rgba::new(85, 107, 47, 255)),
    ("darkorange", Rgba::new(255, 140, 0, 255)),
    ("darkorchid", Rgba::new(153, 50, 204, 255)),
    ("darkred", Rgba::new(139, 0, 0, 255)),
    ("darksalmon", Rgba::new(233, 150, 122, 255)),
    ("darkseagreen", Rgba::new(143, 188, 143, 255)),
    ("darkslateblue", Rgba::new(72, 61, 139, 255)),
    ("darkslategray", Rgba::new(47, 79, 79, 255)),
    ("darkslategrey", Rgba::new(47, 79, 79, 255)),
    ("darkturquoise", Rgba::new(0, 206, 209, 255)),
    ("darkviolet", Rgba::new(148, 0, 211, 255)),
    ("deeppink", Rgba::new(255, 20, 147, 255)),
    ("deepskyblue", Rgba::new(0, 191, 255, 255)),
    ("dimgray", Rgba::new(105, 105, 105, 255)),
    ("dimgrey", Rgba::new(105, 105, 105, 255)),
    ("dodgerblue", Rgba::new(30, 144, 255, 255)),
    ("firebrick", Rgba::new(178, 34, 34, 255)),
    ("floralwhite", Rgba::new(255, 250, 240, 255)),
    ("forestgreen", Rgba::new(34, 139, 34, 255)),
    ("fuchsia", Rgba::new(255, 0, 255, 255)),
    ("gainsboro", Rgba::new(220, 220, 220, 255)),
    ("ghostwhite", Rgba::new(248, 248, 255, 255)),
    ("gold", Rgba::new(255, 215, 0, 255)),
    ("goldenrod", Rgba::new(218, 165, 32, 255)),
    ("gray", Rgba::new(128, 128, 128, 255)),
    ("green", Rgba::new(0, 128, 0, 255)),
    ("greenyellow", Rgba::new(173, 255, 47, 255)),
    ("grey", Rgba::new(128, 128, 128, 255)),
    ("honeydew", Rgba::new(240, 255, 240, 255)),
    ("hotpink", Rgba::new(255, 105, 180, 255)),
    ("indianred", Rgba::new(205, 92, 92, 255)),
    ("indigo", Rgba::new(75, 0, 130, 255)),
    ("ivory", Rgba::new(255, 255, 240, 255)),
    ("khaki", Rgba::new(240, 230, 140, 255)),
    ("lavender", Rgba::new(230, 230, 250, 255)),
    ("lavenderblush", Rgba::new(255, 240, 245, 255)),
    ("lawngreen", Rgba::new(124, 252, 0, 255)),
    ("lemonchiffon", Rgba::new(255, 250, 205, 255)),
    ("lightblue", Rgba::new(173, 216, 230, 255)),
    ("lightcoral", Rgba::new(240, 128, 128, 255)),
    ("lightcyan", Rgba::new(224, 255, 255, 255)),
    ("lightgoldenrodyellow", Rgba::new(250, 250, 210, 255)),
    ("lightgray", Rgba::new(211, 211, 211, 255)),
    ("lightgreen", Rgba::new(144, 238, 144, 255)),
    ("lightgrey", Rgba::new(211, 211, 211, 255)),
    ("lightpink", Rgba::new(255, 182, 193, 255)),
    ("lightsalmon", Rgba::new(255, 160, 122, 255)),
    ("lightseagreen", Rgba::new(32, 178, 170, 255)),
    ("lightskyblue", Rgba::new(135, 206, 250, 255)),
    ("lightslategray", Rgba::new(119, 136, 153, 255)),
    ("lightslategrey", Rgba::new(119, 136, 153, 255)),
    ("lightsteelblue", Rgba::new(176, 196, 222, 255)),
    ("lightyellow", Rgba::new(255, 255, 224, 255)),
    ("lime", Rgba::new(0, 255, 0, 255)),
    ("limegreen", Rgba::new(50, 205, 50, 255)),
    ("linen", Rgba::new(250, 240, 230, 255)),
    ("magenta", Rgba::new(255, 0, 255, 255)),
    ("maroon", Rgba::new(128, 0, 0, 255)),
    ("mediumaquamarine", Rgba::new(102, 205, 170, 255)),
    ("mediumblue", Rgba::new(0, 0, 205, 255)),
    ("mediumorchid", Rgba::new(186, 85, 211, 255)),
    ("mediumpurple", Rgba::new(147, 112, 219, 255)),
    ("mediumseagreen", Rgba::new(60, 179, 113, 255)),
    ("mediumslateblue", Rgba::new(123, 104, 238, 255)),
    ("mediumspringgreen", Rgba::new(0, 250, 154, 255)),
    ("mediumturquoise", Rgba::new(72, 209, 204, 255)),
    ("mediumvioletred", Rgba::new(199, 21, 133, 255)),
    ("midnightblue", Rgba::new(25, 25, 112, 255)),
    ("mintcream", Rgba::new(245, 255, 250, 255)),
    ("mistyrose", Rgba::new(255, 228, 225, 255)),
    ("moccasin", Rgba::new(255, 228, 181, 255)),
    ("navajowhite", Rgba::new(255, 222, 173, 255)),
    ("navy", Rgba::new(0, 0, 128, 255)),
    ("oldlace", Rgba::new(253, 245, 230, 255)),
    ("olive", Rgba::new(128, 128, 0, 255)),
    ("olivedrab", Rgba::new(107, 142, 35, 255)),
    ("orange", Rgba::new(255, 165, 0, 255)),
    ("orangered", Rgba::new(255, 69, 0, 255)),
    ("orchid", Rgba::new(218, 112, 214, 255)),
    ("palegoldenrod", Rgba::new(238, 232, 170, 255)),
    ("palegreen", Rgba::new(152, 251, 152, 255)),
    ("paleturquoise", Rgba::new(175, 238, 238, 255)),
    ("palevioletred", Rgba::new(219, 112, 147, 255)),
    ("papayawhip", Rgba::new(255, 239, 213, 255)),
    ("peachpuff", Rgba::new(255, 218, 185, 255)),
    ("peru", Rgba::new(205, 133, 63, 255)),
    ("pink", Rgba::new(255, 192, 203, 255)),
    ("plum", Rgba::new(221, 160, 221, 255)),
    ("powderblue", Rgba::new(176, 224, 230, 255)),
    ("purple", Rgba::new(128, 0, 128, 255)),
    ("rebeccapurple", Rgba::new(102, 51, 153, 255)),
    ("red", Rgba::new(255, 0, 0, 255)),
    ("rosybrown", Rgba::new(188, 143, 143, 255)),
    ("royalblue", Rgba::new(65, 105, 225, 255)),
    ("saddlebrown", Rgba::new(139, 69, 19, 255)),
    ("salmon", Rgba::new(250, 128, 114, 255)),
    ("sandybrown", Rgba::new(244, 164, 96, 255)),
    ("seagreen", Rgba::new(46, 139, 87, 255)),
    ("seashell", Rgba::new(255, 245, 238, 255)),
    ("sienna", Rgba::new(160, 82, 45, 255)),
    ("silver", Rgba::new(192, 192, 192, 255)),
    ("skyblue", Rgba::new(135, 206, 235, 255)),
    ("slateblue", Rgba::new(106, 90, 205, 255)),
    ("slategray", Rgba::new(112, 128, 144, 255)),
    ("slategrey", Rgba::new(112, 128, 144, 255)),
    ("snow", Rgba::new(255, 250, 250, 255)),
    ("springgreen", Rgba::new(0, 255, 127, 255)),
    ("steelblue", Rgba::new(70, 130, 180, 255)),
    ("tan", Rgba::new(210, 180, 140, 255)),
    ("teal", Rgba::new(0, 128, 128, 255)),
    ("thistle", Rgba::new(216, 191, 216, 255)),
    ("tomato", Rgba::new(255, 99, 71, 255)),
    ("turquoise", Rgba::new(64, 224, 208, 255)),
    ("violet", Rgba::new(238, 130, 238, 255)),
    ("wheat", Rgba::new(245, 222, 179, 255)),
    ("white", Rgba::new(255, 255, 255, 255)),
    ("whitesmoke", Rgba::new(245, 245, 245, 255)),
    ("yellow", Rgba::new(255, 255, 0, 255)),
    ("yellowgreen", Rgba::new(154, 205, 50, 255)),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_colors_are_case_insensitive_and_cover_the_full_table() {
        assert_eq!(parse("red").unwrap(), Rgba::new(255, 0, 0, 255));
        assert_eq!(parse("ReBeCcApUrPlE").unwrap(), Rgba::new(102, 51, 153, 255));
        assert_eq!(parse("cornflowerblue").unwrap(), Rgba::new(100, 149, 237, 255));
        assert_eq!(NAMED_COLORS.len(), 148);
    }

    #[test]
    fn transparent_is_zero_alpha_black() {
        assert_eq!(parse("transparent").unwrap(), Rgba::new(0, 0, 0, 0));
    }

    #[test]
    fn hex_forms_all_agree() {
        assert_eq!(parse("#f00").unwrap(), Rgba::new(255, 0, 0, 255));
        assert_eq!(parse("#f008").unwrap(), Rgba::new(255, 0, 0, 0x88));
        assert_eq!(parse("#ff0000").unwrap(), Rgba::new(255, 0, 0, 255));
        assert_eq!(parse("#FF000080").unwrap(), Rgba::new(255, 0, 0, 0x80));
    }

    #[test]
    fn hex_rejects_bad_digit_counts_and_non_hex_chars() {
        assert!(parse("#ff0").is_ok(), "3 digits is a valid shorthand");
        assert!(parse("#ff000").is_err(), "5 digits is not a valid length");
        assert!(parse("#gg0000").is_err());
    }

    #[test]
    fn rgb_accepts_legacy_comma_and_modern_space_syntax_with_percentages() {
        assert_eq!(parse("rgb(255, 0, 0)").unwrap(), Rgba::new(255, 0, 0, 255));
        assert_eq!(parse("rgba(255, 0, 0, 0.5)").unwrap(), Rgba::new(255, 0, 0, 128));
        assert_eq!(parse("rgb(100% 0% 0%)").unwrap(), Rgba::new(255, 0, 0, 255));
        assert_eq!(parse("rgb(255 0 0 / 50%)").unwrap(), Rgba::new(255, 0, 0, 128));
    }

    #[test]
    fn hsl_matches_known_conversions() {
        assert_eq!(parse("hsl(0, 100%, 50%)").unwrap(), Rgba::new(255, 0, 0, 255));
        assert_eq!(parse("hsl(120deg 100% 50%)").unwrap(), Rgba::new(0, 255, 0, 255));
        assert_eq!(parse("hsl(240 100% 50% / 0.5)").unwrap(), Rgba::new(0, 0, 255, 128));
        assert_eq!(parse("hsl(0.5turn 100% 50%)").unwrap(), Rgba::new(0, 255, 255, 255));
    }

    #[test]
    fn hwb_matches_known_conversions() {
        assert_eq!(parse("hwb(0 0% 0%)").unwrap(), Rgba::new(255, 0, 0, 255));
        assert_eq!(parse("hwb(0 100% 0%)").unwrap(), Rgba::new(255, 255, 255, 255));
        assert_eq!(parse("hwb(0 0% 100%)").unwrap(), Rgba::new(0, 0, 0, 255));
    }

    #[test]
    fn color_mix_in_srgb_splits_evenly_by_default() {
        assert_eq!(parse("color-mix(in srgb, red, blue)").unwrap(), Rgba::new(128, 0, 128, 255));
        assert_eq!(parse("color-mix(in srgb, white, black)").unwrap(), Rgba::new(128, 128, 128, 255));
    }

    #[test]
    fn color_mix_honors_explicit_and_complementary_percentages() {
        assert_eq!(parse("color-mix(in srgb, red 100%, blue 0%)").unwrap(), Rgba::new(255, 0, 0, 255));
        assert_eq!(parse("color-mix(in srgb, red 75%, blue)").unwrap(), Rgba::new(191, 0, 64, 255));
        assert_eq!(parse("color-mix(in srgb, 75% red, blue)").unwrap(), Rgba::new(191, 0, 64, 255));
    }

    #[test]
    fn color_mix_scales_alpha_when_percentages_undersum() {
        // 20% + 20% = 40% of full coverage -- the rest reads as "mixed
        // with nothing", so the result is 40% as opaque as a normal mix.
        let c = parse("color-mix(in srgb, red 20%, blue 20%)").unwrap();
        assert_eq!(c, Rgba::new(128, 0, 128, 102));
    }

    #[test]
    fn color_mix_supports_nested_color_mix_and_hsl_hwb_spaces() {
        assert!(parse("color-mix(in hsl, red, blue)").is_ok());
        assert!(parse("color-mix(in hwb, red, blue)").is_ok());
        assert!(parse("color-mix(in srgb, color-mix(in srgb, red, white), blue)").is_ok());
    }

    #[test]
    fn color_mix_rejects_zero_percentages_and_unknown_spaces() {
        assert!(parse("color-mix(in srgb, red 0%, blue 0%)").is_err());
        assert!(parse("color-mix(in oklch, red, blue)").is_err());
    }

    #[test]
    fn hue_wraps_and_accepts_negative_or_out_of_range_values() {
        assert_eq!(parse("hsl(-120, 100%, 50%)").unwrap(), parse("hsl(240, 100%, 50%)").unwrap());
        assert_eq!(parse("hsl(480, 100%, 50%)").unwrap(), parse("hsl(120, 100%, 50%)").unwrap());
    }

    #[test]
    fn legacy_comma_syntax_tolerates_no_surrounding_whitespace() {
        assert_eq!(parse("rgb(255,0,0)").unwrap(), Rgba::new(255, 0, 0, 255));
    }

    #[test]
    fn to_hex_round_trips_through_parse() {
        let c = parse("cornflowerblue").unwrap();
        assert_eq!(parse(&c.to_hex()).unwrap(), c);
        let translucent = parse("rgba(10, 20, 30, 0.25)").unwrap();
        assert_eq!(parse(&translucent.to_hex()).unwrap(), translucent);
    }

    #[test]
    fn invalid_colors_are_rejected() {
        assert!(parse("").is_err());
        assert!(parse("not-a-color").is_err());
        assert!(parse("rgb(1, 2)").is_err());
        assert!(parse("hsl(0, 50%, 200)").is_err(), "lightness without a % must fail");
        assert!(parse("rgb(1 2 3").is_err(), "unterminated function must fail");
    }

    #[test]
    fn parse_terminal_resolves_named_ansi_slots_and_the_ansi_function() {
        assert_eq!(parse_terminal("-bish-red").unwrap(), TermColor::Ansi(1));
        assert_eq!(parse_terminal("-bish-bright-white").unwrap(), TermColor::Ansi(15));
        assert_eq!(parse_terminal("-bish-ansi(200)").unwrap(), TermColor::Ansi(200));
        assert_eq!(parse_terminal("-bish-ansi(0)").unwrap(), TermColor::Ansi(0));
    }

    #[test]
    fn parse_terminal_falls_through_to_an_ordinary_rgba_color() {
        assert_eq!(parse_terminal("cornflowerblue").unwrap(), TermColor::Rgba(Rgba::new(100, 149, 237, 255)));
        assert_eq!(parse_terminal("#ff0000").unwrap(), TermColor::Rgba(Rgba::new(255, 0, 0, 255)));
    }

    #[test]
    fn parse_terminal_rejects_unknown_vendor_names_and_out_of_range_indices() {
        assert!(parse_terminal("-bish-not-a-real-slot").is_err());
        assert!(parse_terminal("-bish-ansi(256)").is_err());
        assert!(parse_terminal("-bish-ansi(nope)").is_err());
        assert!(parse_terminal("-bish-rgb(1, 2, 3)").is_err(), "only -bish-ansi(...) is a vendor function, not every function under -bish-");
    }

    #[test]
    fn a_vendor_terminal_color_cannot_be_used_inside_color_mix() {
        // color-mix() parses its own component colors via `parse`, not
        // `parse_terminal` -- there's no fixed RGB to mix a terminal
        // palette slot with, so this must fail rather than silently
        // treating "-bish-red" as an unrecognized (and thus ignored)
        // color function.
        assert!(parse("color-mix(in srgb, -bish-red, blue)").is_err());
    }
}
