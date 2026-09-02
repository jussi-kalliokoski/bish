// Wall-clock time, and the small `strftime` subset this shell needs to
// print it.
//
// It lives in its own file for one reason: findability. Every user of a
// date is somewhere else -- `PS1`'s `\\d`/`\\t`/`\\T`/`\\@`/`\\A`/`\\D{...}`
// and `${v@P}` in exec.rs, `printf %(FORMAT)T` in exec.rs, history
// timestamps in history.rs, `git log` dates in git.rs -- and while this
// was a private `fn` sixteen thousand lines into exec.rs, the only way
// to discover that bish already formats time was to grep for the exact
// C name. A search for "date" or "localtime_r" found nothing useful.
//
// No date crate: the broken-down time comes from libc's `localtime_r`
// through the same raw FFI pattern the rest of this codebase uses, and
// the formatting is written out here.

// The glibc/BSD `struct tm` layout (POSIX's 9 base fields plus the
// common tm_gmtoff/tm_zone extension both platforms agree on) --
// localtime_r writes a full struct tm's worth of bytes into its output
// pointer regardless of what this declares, so this has to match the
// real platform layout size-for-size, not just the fields this code
// actually reads.
#[repr(C)]
pub(crate) struct CTm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    tm_gmtoff: i64,
    tm_zone: *const i8,
}

// `${v@P}`'s `\d`/`\t`/`\T`/`\@`/`\A`/`\D{...}` all need the current
// local wall-clock time -- computed via the same raw libc FFI pattern
// already used elsewhere in this file (e.g. stdin_is_tty/stdin_ready),
// rather than pulling in a date/time crate for it.
// The broken-down local time for an arbitrary timestamp, where
// `local_time_now` only ever answers for "right now" -- what
// `printf %(...)T` and `HISTTIMEFORMAT` both need.
pub(crate) fn local_time_at(epoch_secs: i64) -> CTm {
    unsafe extern "C" {
        fn localtime_r(t: *const i64, result: *mut CTm) -> *mut CTm;
    }
    let mut tm = CTm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: std::ptr::null(),
    };
    unsafe { localtime_r(&epoch_secs as *const i64, &mut tm as *mut CTm) };
    tm
}

pub(crate) fn local_time_now() -> CTm {
    unsafe extern "C" {
        fn time(t: *mut i64) -> i64;
        fn localtime_r(t: *const i64, result: *mut CTm) -> *mut CTm;
    }
    let mut t: i64 = 0;
    unsafe { time(&mut t as *mut i64) };
    let mut tm = CTm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: std::ptr::null(),
    };
    unsafe { localtime_r(&t as *const i64, &mut tm as *mut CTm) };
    tm
}

const WEEKDAY_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const WEEKDAY_FULL: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTH_ABBR: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const MONTH_FULL: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

// `\d`: bash's own default (no-arg) date format, "Weekday Month Day"
// with the day space-padded to two columns (matching `%e`, e.g. "Tue
// May  6" for the 6th) -- no year, no locale support (bish has none at
// all), always English abbreviations.
pub(crate) fn prompt_date() -> String {
    let tm = local_time_now();
    format!("{} {} {:2}", WEEKDAY_ABBR[tm.tm_wday as usize % 7], MONTH_ABBR[tm.tm_mon as usize % 12], tm.tm_mday)
}

// A small strftime subset covering the specifiers a prompt format
// string would plausibly use -- not a general-purpose implementation
// (no locale support, no width/padding modifiers beyond what's baked
// into each specifier below). An unrecognized `%X` passes through
// literally, matching this codebase's own established convention for
// an unrecognized escape sequence elsewhere (e.g. expand_backslash_escapes).
pub(crate) fn strftime(fmt: &str, tm: &CTm) -> String {
    strftime_at(fmt, tm, None)
}

// The zone abbreviation the C library put in `tm_zone` ("UTC", "EEST",
// ...). A borrowed C string that libc owns and keeps alive, so this
// copies it out rather than holding the pointer.
pub(crate) fn tm_zone_name(tm: &CTm) -> String {
    if tm.tm_zone.is_null() {
        return String::new();
    }
    let mut out = String::new();
    let mut p = tm.tm_zone;
    // Bounded: a zone abbreviation is a handful of bytes, and a
    // runaway pointer must not become a runaway loop.
    for _ in 0..32 {
        let byte = unsafe { *p };
        if byte == 0 {
            break;
        }
        out.push(byte as u8 as char);
        p = unsafe { p.add(1) };
    }
    out
}

// The same, with the timestamp `tm` was derived from -- `%s` is the one
// directive that cannot be recovered from a broken-down time without
// re-deriving the calendar, and the callers that want it always have it.
// `None` prints `%s` literally rather than guessing.
pub(crate) fn strftime_at(fmt: &str, tm: &CTm, epoch: Option<i64>) -> String {
    let year = tm.tm_year + 1900;
    let hour24 = tm.tm_hour;
    let hour12 = match hour24 % 12 {
        0 => 12,
        h => h,
    };
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(spec) = chars.next() else {
            out.push('%');
            break;
        };
        match spec {
            'Y' => out.push_str(&year.to_string()),
            'y' => out.push_str(&format!("{:02}", year.rem_euclid(100))),
            'm' => out.push_str(&format!("{:02}", tm.tm_mon + 1)),
            'd' => out.push_str(&format!("{:02}", tm.tm_mday)),
            'e' => out.push_str(&format!("{:2}", tm.tm_mday)),
            'H' => out.push_str(&format!("{:02}", hour24)),
            'I' => out.push_str(&format!("{:02}", hour12)),
            'M' => out.push_str(&format!("{:02}", tm.tm_min)),
            'S' => out.push_str(&format!("{:02}", tm.tm_sec)),
            'p' => out.push_str(if hour24 < 12 { "AM" } else { "PM" }),
            'a' => out.push_str(WEEKDAY_ABBR[tm.tm_wday as usize % 7]),
            'A' => out.push_str(WEEKDAY_FULL[tm.tm_wday as usize % 7]),
            'b' => out.push_str(MONTH_ABBR[tm.tm_mon as usize % 12]),
            'B' => out.push_str(MONTH_FULL[tm.tm_mon as usize % 12]),
            'j' => out.push_str(&format!("{:03}", tm.tm_yday + 1)),
            'T' => out.push_str(&format!("{:02}:{:02}:{:02}", hour24, tm.tm_min, tm.tm_sec)),
            'F' => out.push_str(&format!("{:04}-{:02}-{:02}", year, tm.tm_mon + 1, tm.tm_mday)),
            'R' => out.push_str(&format!("{:02}:{:02}", hour24, tm.tm_min)),
            'D' => out.push_str(&format!("{:02}/{:02}/{:02}", tm.tm_mon + 1, tm.tm_mday, year.rem_euclid(100))),
            'C' => out.push_str(&format!("{:02}", year.div_euclid(100))),
            'u' => out.push_str(&(if tm.tm_wday == 0 { 7 } else { tm.tm_wday }).to_string()),
            'w' => out.push_str(&tm.tm_wday.to_string()),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            // The offset the C library resolved for *this* timestamp, so
            // a summer date formats with summer's offset rather than
            // today's -- which is the whole reason `tm_gmtoff` is filled
            // in per-conversion.
            'z' => {
                let (sign, off) = if tm.tm_gmtoff < 0 { ('-', -tm.tm_gmtoff) } else { ('+', tm.tm_gmtoff) };
                out.push_str(&format!("{sign}{:02}{:02}", off / 3600, (off % 3600) / 60));
            }
            'Z' => out.push_str(&tm_zone_name(tm)),
            's' => match epoch {
                Some(secs) => out.push_str(&secs.to_string()),
                None => out.push_str("%s"),
            },
            '%' => out.push('%'),
            other => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Checked against the system's own `date(1)`, on the same
    // timestamps and in whatever zone this machine is in -- the same
    // "test against the real thing, not against my reading of the spec"
    // rule the shell's own bash-comparison tests follow. Skipped where
    // there is no `date`, the way the git and inflate tests skip.
    #[test]
    fn strftime_agrees_with_the_system_date() {
        const SPECS: &str = "%Y|%y|%m|%d|%e|%H|%I|%M|%S|%p|%a|%A|%b|%B|%j|%T|%F|%R|%D|%C|%u|%w|%z|%Z|%s";
        for epoch in [0i64, 1_000_000_000, 1_700_000_000, 2_000_000_000] {
            // `LC_ALL=C` because this `strftime` has no locale support
            // and says so -- the names it prints are always English.
            // Without it the comparison measures the machine's locale
            // rather than the formatting (a Finnish `date` answers
            // "torstai" for %A and nothing at all for %p).
            let out = std::process::Command::new("date").env("LC_ALL", "C").arg(format!("-d@{epoch}")).arg(format!("+{SPECS}")).output();
            let Ok(out) = out else { return };
            if !out.status.success() {
                return;
            }
            let want = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
            let got = strftime_at(SPECS, &local_time_at(epoch), Some(epoch));
            assert_eq!(got, want, "at epoch {epoch}");
        }
    }

    // An unrecognized specifier passes through literally rather than
    // being swallowed -- the same convention `expand_backslash_escapes`
    // follows, and the reason a `PS1` with a stray `%` is readable
    // rather than mysteriously shorter.
    #[test]
    fn an_unknown_specifier_survives_as_itself() {
        let tm = local_time_at(0);
        assert_eq!(strftime("%Q", &tm), "%Q");
        assert_eq!(strftime("100%%", &tm), "100%");
        assert_eq!(strftime("trailing %", &tm), "trailing %");
        // `%s` needs the timestamp, which `strftime` does not carry --
        // it says so rather than inventing one.
        assert_eq!(strftime("%s", &tm), "%s");
        assert_eq!(strftime_at("%s", &tm, Some(42)), "42");
    }
}
