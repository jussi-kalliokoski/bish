// Wall-clock time and the `strftime` subset bish needs, for prompt
// escapes (`\d`, `\t`, `\D{...}`), `printf %(...)T`, `HISTTIMEFORMAT`
// and `git log` dates. The broken-down time comes from the platform layer.

/// C's `struct tm`.
pub(crate) use crate::platform::CTm;

pub(crate) fn local_time_at(epoch_secs: i64) -> CTm {
    crate::platform::local_time_at(epoch_secs)
}

pub(crate) fn local_time_now() -> CTm {
    crate::platform::local_time_at(crate::platform::epoch_seconds())
}

const WEEKDAY_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const WEEKDAY_FULL: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTH_ABBR: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const MONTH_FULL: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

// `\d`: bash's "Tue May  6": no year, the day padded like `%e`.
pub(crate) fn prompt_date() -> String {
    let tm = local_time_now();
    format!("{} {} {:2}", WEEKDAY_ABBR[tm.tm_wday as usize % 7], MONTH_ABBR[tm.tm_mon as usize % 12], tm.tm_mday)
}

// No locale support (names are English) and no width modifiers. An
// unknown `%X` is printed as is, like an unknown backslash escape.
pub(crate) fn strftime(fmt: &str, tm: &CTm) -> String {
    strftime_at(fmt, tm, None)
}

// `tm_zone` ("UTC", "EEST"), copied out of the C library's string.
pub(crate) fn tm_zone_name(tm: &CTm) -> String {
    if tm.tm_zone.is_null() {
        return String::new();
    }
    let mut out = String::new();
    let mut p = tm.tm_zone;
    // Bounded, in case the pointer is bad.
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

// `%s` needs the timestamp `tm` came from; with `None` it prints `%s`.
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
            // This timestamp's offset, so a summer date gets summer's.
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

    // Skipped where there is no `date`.
    #[test]
    fn strftime_agrees_with_the_system_date() {
        const SPECS: &str = "%Y|%y|%m|%d|%e|%H|%I|%M|%S|%p|%a|%A|%b|%B|%j|%T|%F|%R|%D|%C|%u|%w|%z|%Z|%s";
        for epoch in [0i64, 1_000_000_000, 1_700_000_000, 2_000_000_000] {
            // Without `LC_ALL=C` a Finnish `date` says "torstai" for %A.
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

    #[test]
    fn an_unknown_specifier_survives_as_itself() {
        let tm = local_time_at(0);
        assert_eq!(strftime("%Q", &tm), "%Q");
        assert_eq!(strftime("100%%", &tm), "100%");
        assert_eq!(strftime("trailing %", &tm), "trailing %");
        assert_eq!(strftime("%s", &tm), "%s");
        assert_eq!(strftime_at("%s", &tm, Some(42)), "42");
    }
}
