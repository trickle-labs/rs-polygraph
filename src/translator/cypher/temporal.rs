// ── Temporal Truncation helpers ───────────────────────────────────────────────

/// Decomposed temporal components used during truncation.
#[derive(Clone, Debug, Default)]
pub(crate) struct TcComponents {
    pub(crate) year: Option<i64>,
    pub(crate) month: Option<i64>,
    pub(crate) day: Option<i64>,
    pub(crate) hour: Option<i64>,
    pub(crate) minute: Option<i64>,
    pub(crate) second: Option<i64>,
    /// Combined nanosecond within second (0..=999_999_999). None = not specified.
    pub(crate) ns: Option<i64>,
    /// Full timezone suffix appended to output, e.g. "Z", "+01:00",
    /// "+01:00[Europe/Stockholm]". None = local/no-timezone.
    pub(crate) tz: Option<String>,
    /// True when the source temporal expression was `localdatetime()` or `localtime()`.
    /// Drives the Z-suffix rule for `localdatetime.truncate` at time-granularity units.
    #[allow(dead_code)]
    pub(crate) is_localdatetime: bool,
}

/// Parse a TcComponents from an ISO-8601 temporal string value.
/// Used by the LQA truncation path where the temporal value was already
/// folded to a typed SPARQL literal string.
pub(crate) fn tc_from_iso_string(s: &str) -> Option<TcComponents> {
    if let Some(t_pos) = s.find('T') {
        // DateTime or LocalDateTime: YYYY-MM-DDTHH:MM:SS[.frac][tz]
        let date_part = &s[..t_pos];
        let rest = &s[t_pos + 1..];
        let y: i64 = date_part.get(0..4)?.parse().ok()?;
        let m: i64 = date_part.get(5..7)?.parse().ok()?;
        let d: i64 = date_part.get(8..10)?.parse().ok()?;
        let h: i64 = rest.get(0..2)?.parse().ok()?;
        let min: i64 = rest.get(3..5)?.parse().ok()?;
        let sec: i64 = rest.get(6..8)?.parse().ok()?;
        // Nanoseconds and trailing timezone
        let (ns_opt, tz_opt) = tc_parse_sub_second_and_tz(rest.get(8..)?);
        Some(TcComponents {
            year: Some(y),
            month: Some(m),
            day: Some(d),
            hour: Some(h),
            minute: Some(min),
            second: Some(sec),
            ns: ns_opt,
            tz: tz_opt,
            ..Default::default()
        })
    } else if s.len() >= 8 && s.as_bytes().get(2) == Some(&b':') {
        // Time only: HH:MM:SS[.frac][tz]
        let h: i64 = s.get(0..2)?.parse().ok()?;
        let min: i64 = s.get(3..5)?.parse().ok()?;
        let sec: i64 = s.get(6..8)?.parse().ok()?;
        let (ns_opt, tz_opt) = tc_parse_sub_second_and_tz(s.get(8..)?);
        Some(TcComponents {
            hour: Some(h),
            minute: Some(min),
            second: Some(sec),
            ns: ns_opt,
            tz: tz_opt,
            ..Default::default()
        })
    } else {
        // Date: YYYY-MM-DD
        let y: i64 = s.get(0..4)?.parse().ok()?;
        let m: i64 = s.get(5..7)?.parse().ok()?;
        let d: i64 = s.get(8..10)?.parse().ok()?;
        Some(TcComponents {
            year: Some(y),
            month: Some(m),
            day: Some(d),
            ..Default::default()
        })
    }
}

/// Parse optional fractional-seconds and timezone from the tail of a time string.
/// The `tail` starts right after `HH:MM:SS` (e.g. `".123456789Z"` or `"+05:30"`).
fn tc_parse_sub_second_and_tz(tail: &str) -> (Option<i64>, Option<String>) {
    if tail.is_empty() {
        return (None, None);
    }
    if tail.starts_with('.') {
        // Find end of digit run after the dot
        let frac_end = 1 + tail[1..].find(|c: char| !c.is_ascii_digit()).unwrap_or(tail.len() - 1);
        let frac = &tail[1..frac_end];
        let mut buf = frac.to_string();
        while buf.len() < 9 {
            buf.push('0');
        }
        let ns: i64 = buf[..9].parse().unwrap_or(0);
        let tz = {
            let after = &tail[frac_end..];
            if after.is_empty() { None } else { Some(after.to_string()) }
        };
        (Some(ns), tz)
    } else {
        (None, Some(tail.to_string()))
    }
}

/// Extract TcComponents from a literal temporal function-call expression.
pub(crate) fn tc_from_expr(expr: &Expression) -> Option<TcComponents> {
    let Expression::FunctionCall { name, args, .. } = expr else {
        return None;
    };
    let nm = name.to_ascii_lowercase();
    match nm.as_str() {
        "date" => {
            if let Some(Expression::Map(pairs)) = args.first() {
                let year = temporal_get_i(pairs, "year")?;
                let (month, day) = tc_extract_date_md(year, pairs);
                Some(TcComponents {
                    year: Some(year),
                    month: Some(month),
                    day: Some(day),
                    ..Default::default()
                })
            } else if let Some(Expression::Literal(Literal::String(s))) = args.first() {
                let ds = temporal_parse_date(s)?;
                let y = ds[..4].parse().ok()?;
                let m = ds[5..7].parse().ok()?;
                let d = ds[8..10].parse().ok()?;
                Some(TcComponents {
                    year: Some(y),
                    month: Some(m),
                    day: Some(d),
                    ..Default::default()
                })
            } else {
                None
            }
        }
        "localdatetime" => {
            if let Some(Expression::Map(pairs)) = args.first() {
                let year = temporal_get_i(pairs, "year")?;
                let (month, day) = tc_extract_date_md(year, pairs);
                let hour = temporal_get_i(pairs, "hour");
                let minute = temporal_get_i(pairs, "minute");
                let second = temporal_get_i(pairs, "second");
                let ns = tc_extract_ns(pairs);
                Some(TcComponents {
                    year: Some(year),
                    month: Some(month),
                    day: Some(day),
                    hour,
                    minute,
                    second,
                    ns,
                    is_localdatetime: true,
                    ..Default::default()
                })
            } else {
                None
            }
        }
        "datetime" => {
            if let Some(Expression::Map(pairs)) = args.first() {
                let year = temporal_get_i(pairs, "year")?;
                let (month, day) = tc_extract_date_md(year, pairs);
                let hour = temporal_get_i(pairs, "hour");
                let minute = temporal_get_i(pairs, "minute");
                let second = temporal_get_i(pairs, "second");
                let ns = tc_extract_ns(pairs);
                let tz = temporal_get_s(pairs, "timezone").map(|s| tc_tz_suffix(&s));
                Some(TcComponents {
                    year: Some(year),
                    month: Some(month),
                    day: Some(day),
                    hour,
                    minute,
                    second,
                    ns,
                    tz,
                    ..Default::default()
                })
            } else {
                None
            }
        }
        "localtime" => {
            if let Some(Expression::Map(pairs)) = args.first() {
                let hour = temporal_get_i(pairs, "hour")?;
                let minute = temporal_get_i(pairs, "minute");
                let second = temporal_get_i(pairs, "second");
                let ns = tc_extract_ns(pairs);
                Some(TcComponents {
                    hour: Some(hour),
                    minute,
                    second,
                    ns,
                    is_localdatetime: true, // localtime → Z rule
                    ..Default::default()
                })
            } else {
                None
            }
        }
        "time" => {
            if let Some(Expression::Map(pairs)) = args.first() {
                let hour = temporal_get_i(pairs, "hour")?;
                let minute = temporal_get_i(pairs, "minute");
                let second = temporal_get_i(pairs, "second");
                let ns = tc_extract_ns(pairs);
                let tz = temporal_get_s(pairs, "timezone").map(|s| tc_tz_suffix(&s));
                Some(TcComponents {
                    hour: Some(hour),
                    minute,
                    second,
                    ns,
                    tz,
                    ..Default::default()
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Extract (month, day) from map pairs given year, handling calendar variants.
fn tc_extract_date_md(year: i64, pairs: &[(String, Expression)]) -> (i64, i64) {
    if let Some(m) = temporal_get_i(pairs, "month") {
        let d = temporal_get_i(pairs, "day").unwrap_or(1);
        return (m, d);
    }
    if let Some(w) = temporal_get_i(pairs, "week") {
        let dow = temporal_get_i(pairs, "dayOfWeek").unwrap_or(1);
        let ds = temporal_week_to_date(year, w, dow);
        if let (Ok(m), Ok(d)) = (ds[5..7].parse::<i64>(), ds[8..10].parse::<i64>()) {
            return (m, d);
        }
    }
    if let Some(ord) = temporal_get_i(pairs, "ordinalDay") {
        let (m, d) = temporal_ordinal_to_md(year, ord);
        return (m, d);
    }
    if let Some(q) = temporal_get_i(pairs, "quarter") {
        let doq = temporal_get_i(pairs, "dayOfQuarter").unwrap_or(1);
        let (m, d) = temporal_quarter_to_md(year, q, doq);
        return (m, d);
    }
    (1, 1)
}

/// Extract combined nanosecond value from map pairs (millisecond + microsecond + nanosecond).
fn tc_extract_ns(pairs: &[(String, Expression)]) -> Option<i64> {
    let has_ms = pairs
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("millisecond"));
    let has_us = pairs
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("microsecond"));
    let has_ns = pairs
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("nanosecond"));
    if !has_ms && !has_us && !has_ns {
        return None;
    }
    let ms = temporal_get_i(pairs, "millisecond").unwrap_or(0);
    let us = temporal_get_i(pairs, "microsecond").unwrap_or(0);
    let ns = temporal_get_i(pairs, "nanosecond").unwrap_or(0);
    Some(ms * 1_000_000 + us * 1_000 + ns)
}

/// Normalise a timezone string to a display suffix.
/// Numeric offsets pass through; named timezones are looked up.
fn tc_tz_suffix(tz: &str) -> String {
    tc_tz_suffix_month(tz, 1) // default to January (winter)
}

/// DST-aware timezone suffix: `month` (1-12) used to determine winter/summer offset.
fn tc_tz_suffix_month(tz: &str, month: i64) -> String {
    if tz == "Z" || tz.starts_with('+') || tz.starts_with('-') {
        // Strip trailing ":00" seconds from timezone offset when seconds are zero:
        // "+02:05:00" → "+02:05", "+02:05:59" → "+02:05:59"
        if tz != "Z" && tz.len() == 9 && tz.as_bytes().get(6) == Some(&b':') && tz.ends_with(":00")
        {
            return tz[..6].to_string();
        }
        return tz.to_string();
    }
    // Named timezone lookup — approximate DST by month:
    // Central European Time: +01:00 (Oct-Mar), +02:00 (Apr-Sep)
    let is_summer = matches!(month, 4 | 5 | 6 | 7 | 8 | 9);
    let (winter, summer) = tc_tz_winter_summer(tz);
    let offset = if is_summer { summer } else { winter };
    if offset == "Z" {
        format!("Z[{}]", tz)
    } else {
        format!("{}[{}]", offset, tz)
    }
}

/// Precise DST-aware timezone suffix using year/month/day to determine the exact DST boundary.
/// Uses last-Sunday-of-March / last-Sunday-of-October rule for European timezones.
pub(crate) fn tc_tz_suffix_ymd(tz: &str, y: i64, m: i64, d: i64) -> String {
    tc_tz_suffix_ymdh(tz, y, m, d, 12) // default to noon (avoids transition boundary)
}

/// Precise DST-aware timezone suffix using year/month/day/hour to determine the exact DST
/// boundary.  On the DST transition day, uses the hour to resolve the ambiguity:
/// - Spring (last Sunday of March): clocks advance at 2 AM local → before 2 = winter, ≥2 = summer
/// - Fall  (last Sunday of Oct/Sep): clocks fall back at 3 AM summer → before 3 = summer, ≥3 = winter
pub(crate) fn tc_tz_suffix_ymdh(tz: &str, y: i64, m: i64, d: i64, h: i64) -> String {
    if tz == "Z" || tz.starts_with('+') || tz.starts_with('-') {
        if tz != "Z" && tz.len() == 9 && tz.as_bytes().get(6) == Some(&b':') && tz.ends_with(":00")
        {
            return tz[..6].to_string();
        }
        return tz.to_string();
    }
    let (winter, summer) = tc_tz_winter_summer(tz);
    let is_summer = tc_is_eu_dst_h(tz, y, m, d, h);
    let offset = if is_summer { summer } else { winter };
    if offset == "Z" {
        format!("Z[{}]", tz)
    } else {
        format!("{}[{}]", offset, tz)
    }
}

/// Return the (winter_offset, summer_offset) pair for a named timezone.
fn tc_tz_winter_summer(tz: &str) -> (&'static str, &'static str) {
    match tz {
        "Europe/Stockholm" | "Europe/Paris" | "Europe/Berlin" | "Europe/Rome" | "Europe/Madrid"
        | "Europe/Amsterdam" | "Europe/Brussels" | "Europe/Copenhagen" | "Europe/Warsaw"
        | "Europe/Vienna" | "Europe/Zurich" | "Europe/Prague" | "Europe/Budapest" => {
            ("+01:00", "+02:00")
        }
        "Europe/London" | "Europe/Dublin" | "Europe/Lisbon" => ("Z", "+01:00"),
        "UTC" | "Etc/UTC" => ("Z", "Z"),
        "America/New_York" | "America/Toronto" | "America/Detroit" => ("-05:00", "-04:00"),
        "America/Los_Angeles" | "America/San_Francisco" => ("-08:00", "-07:00"),
        "Asia/Tokyo" => ("+09:00", "+09:00"), // Japan no DST
        "Asia/Shanghai" | "Asia/Beijing" | "Asia/Hong_Kong" => ("+08:00", "+08:00"),
        "Pacific/Honolulu" | "Pacific/Johnston" => ("-10:00", "-10:00"), // Hawaii, no DST
        "Australia/Eucla" => ("+08:45", "+08:45"), // Western Central Standard Time, no DST
        _ => ("Z", "Z"),
    }
}

/// Determine if a European-DST timezone is currently observing DST for the given date.
/// Uses historically-approximate rules:
/// - Spring forward: last Sunday of March (consistent since DST adoption)
/// - Fall back: last Sunday of September (before 1996) or last Sunday of October (1996+)
fn tc_is_eu_dst(tz: &str, y: i64, m: i64, d: i64) -> bool {
    match tz {
        "Europe/Stockholm" | "Europe/Paris" | "Europe/Berlin" | "Europe/Rome" | "Europe/Madrid"
        | "Europe/Amsterdam" | "Europe/Brussels" | "Europe/Copenhagen" | "Europe/Warsaw"
        | "Europe/Vienna" | "Europe/Zurich" | "Europe/Prague" | "Europe/Budapest"
        | "Europe/London" | "Europe/Dublin" | "Europe/Lisbon" => {}
        _ => return false,
    }
    // EU harmonized to last-Sunday-of-October fall-back from 1996 onward;
    // before that, most countries used last Sunday of September.
    let fall_month: i64 = if y >= 1996 { 10 } else { 9 };
    if m > 3 && m < fall_month {
        return true;
    }
    if m < 3 || m > fall_month {
        return false;
    }
    let last_sun = tc_last_sunday_of_month(y, m);
    if m == 3 {
        d >= last_sun // spring forward: from last Sunday of March onward
    } else {
        d < last_sun // fall back: before last Sunday of fall_month
    }
}

/// Hour-aware EU DST check.  Identical to [`tc_is_eu_dst`] except that on the
/// exact transition day it uses the local wall-clock hour to resolve the
/// ambiguity around the clock change:
/// - Spring (last Sunday of March): advance at **2 AM** local → h < 2 = winter, h ≥ 2 = summer
/// - Fall   (last Sunday of Oct/Sep): fall back at **3 AM** summer → h < 3 = summer, h ≥ 3 = winter
fn tc_is_eu_dst_h(tz: &str, y: i64, m: i64, d: i64, h: i64) -> bool {
    match tz {
        "Europe/Stockholm" | "Europe/Paris" | "Europe/Berlin" | "Europe/Rome" | "Europe/Madrid"
        | "Europe/Amsterdam" | "Europe/Brussels" | "Europe/Copenhagen" | "Europe/Warsaw"
        | "Europe/Vienna" | "Europe/Zurich" | "Europe/Prague" | "Europe/Budapest"
        | "Europe/London" | "Europe/Dublin" | "Europe/Lisbon" => {}
        _ => return false,
    }
    let fall_month: i64 = if y >= 1996 { 10 } else { 9 };
    if m > 3 && m < fall_month {
        return true;
    }
    if m < 3 || m > fall_month {
        return false;
    }
    let last_sun = tc_last_sunday_of_month(y, m);
    if m == 3 {
        if d < last_sun { return false; }
        if d > last_sun { return true; }
        // On the transition day: clocks spring forward at 2 AM local.
        h >= 2
    } else {
        // fall_month
        if d < last_sun { return true; }
        if d > last_sun { return false; }
        // On the transition day: clocks fall back at 3 AM local (summer time).
        h < 3
    }
}

/// Return the day (1-based) of the last Sunday in the given month/year.
fn tc_last_sunday_of_month(y: i64, m: i64) -> i64 {
    let last_day = temporal_dim(y, m);
    let epoch = temporal_epoch(y, m, last_day);
    // dow: 1=Mon, 7=Sun (same convention as tc_iso_week_year)
    let dow = ((epoch - 1) % 7 + 7) % 7 + 1;
    let days_back = if dow == 7 { 0 } else { dow };
    last_day - days_back
}

/// Return the ISO week-numbering year for a given calendar date.
fn tc_iso_week_year(y: i64, m: i64, d: i64) -> i64 {
    let epoch = temporal_epoch(y, m, d);
    let dow = ((epoch - 1) % 7 + 7) % 7 + 1; // 1=Mon, 7=Sun
                                             // ISO week year = year of the Thursday in the same ISO week
    let thu_epoch = epoch + (4 - dow);
    temporal_from_epoch(thu_epoch).0
}

/// Apply unit-based truncation to a TcComponents in-place.
pub(crate) fn tc_apply_truncation(unit: &str, comps: &mut TcComponents) {
    match unit {
        "millennium" => {
            if let Some(y) = comps.year {
                comps.year = Some((y / 1000) * 1000);
            }
            comps.month = Some(1);
            comps.day = Some(1);
            comps.hour = Some(0);
            comps.minute = Some(0);
            comps.second = Some(0);
            comps.ns = Some(0);
        }
        "century" => {
            if let Some(y) = comps.year {
                comps.year = Some((y / 100) * 100);
            }
            comps.month = Some(1);
            comps.day = Some(1);
            comps.hour = Some(0);
            comps.minute = Some(0);
            comps.second = Some(0);
            comps.ns = Some(0);
        }
        "decade" => {
            if let Some(y) = comps.year {
                comps.year = Some((y / 10) * 10);
            }
            comps.month = Some(1);
            comps.day = Some(1);
            comps.hour = Some(0);
            comps.minute = Some(0);
            comps.second = Some(0);
            comps.ns = Some(0);
        }
        "year" => {
            comps.month = Some(1);
            comps.day = Some(1);
            comps.hour = Some(0);
            comps.minute = Some(0);
            comps.second = Some(0);
            comps.ns = Some(0);
        }
        "weekYear" => {
            if let (Some(y), Some(m), Some(d)) = (comps.year, comps.month, comps.day) {
                let wy = tc_iso_week_year(y, m, d);
                let mon_str = temporal_week_to_date(wy, 1, 1);
                if let (Ok(ny), Ok(nm), Ok(nd)) = (
                    mon_str[..4].parse::<i64>(),
                    mon_str[5..7].parse::<i64>(),
                    mon_str[8..10].parse::<i64>(),
                ) {
                    comps.year = Some(ny);
                    comps.month = Some(nm);
                    comps.day = Some(nd);
                }
            }
            comps.hour = Some(0);
            comps.minute = Some(0);
            comps.second = Some(0);
            comps.ns = Some(0);
        }
        "quarter" => {
            if let Some(m) = comps.month {
                comps.month = Some(((m - 1) / 3) * 3 + 1);
            }
            comps.day = Some(1);
            comps.hour = Some(0);
            comps.minute = Some(0);
            comps.second = Some(0);
            comps.ns = Some(0);
        }
        "month" => {
            comps.day = Some(1);
            comps.hour = Some(0);
            comps.minute = Some(0);
            comps.second = Some(0);
            comps.ns = Some(0);
        }
        "week" => {
            if let (Some(y), Some(m), Some(d)) = (comps.year, comps.month, comps.day) {
                let epoch = temporal_epoch(y, m, d);
                let dow = ((epoch - 1) % 7 + 7) % 7 + 1; // 1=Mon..7=Sun
                let monday_epoch = epoch - (dow - 1);
                let (ny, nm, nd) = temporal_from_epoch(monday_epoch);
                comps.year = Some(ny);
                comps.month = Some(nm);
                comps.day = Some(nd);
            }
            comps.hour = Some(0);
            comps.minute = Some(0);
            comps.second = Some(0);
            comps.ns = Some(0);
        }
        "day" => {
            comps.hour = Some(0);
            comps.minute = Some(0);
            comps.second = Some(0);
            comps.ns = Some(0);
        }
        "hour" => {
            comps.minute = Some(0);
            comps.second = Some(0);
            comps.ns = Some(0);
        }
        "minute" => {
            comps.second = Some(0);
            comps.ns = Some(0);
        }
        "second" => {
            comps.ns = Some(0);
        }
        "millisecond" => {
            if let Some(n) = comps.ns {
                comps.ns = Some((n / 1_000_000) * 1_000_000);
            }
        }
        "microsecond" => {
            if let Some(n) = comps.ns {
                comps.ns = Some((n / 1_000) * 1_000);
            }
        }
        "nanosecond" => {
            // no truncation
        }
        _ => {}
    }
}

/// Extract integer override value from an Expression.
fn tc_get_override_i(v: &Expression) -> Option<i64> {
    match v {
        Expression::Literal(Literal::Integer(n)) => Some(*n),
        Expression::Literal(Literal::Float(f)) => Some(*f as i64),
        _ => None,
    }
}

/// Apply map override values to TcComponents.
pub(crate) fn tc_apply_overrides(overrides: &[(String, Expression)], comps: &mut TcComponents) {
    for (k, v) in overrides {
        match k.to_ascii_lowercase().as_str() {
            "year" => {
                if let Some(n) = tc_get_override_i(v) {
                    comps.year = Some(n);
                }
            }
            "month" => {
                if let Some(n) = tc_get_override_i(v) {
                    comps.month = Some(n);
                }
            }
            "day" => {
                if let Some(n) = tc_get_override_i(v) {
                    comps.day = Some(n);
                }
            }
            "dayofweek" => {
                // dayOfWeek override: advance from current Monday by (dow-1) days
                if let (Some(y), Some(m), Some(d), Some(dow)) =
                    (comps.year, comps.month, comps.day, tc_get_override_i(v))
                {
                    let monday_epoch = temporal_epoch(y, m, d);
                    let target_epoch = monday_epoch + dow - 1;
                    let (ny, nm, nd) = temporal_from_epoch(target_epoch);
                    comps.year = Some(ny);
                    comps.month = Some(nm);
                    comps.day = Some(nd);
                }
            }
            "hour" => {
                if let Some(n) = tc_get_override_i(v) {
                    comps.hour = Some(n);
                }
            }
            "minute" => {
                if let Some(n) = tc_get_override_i(v) {
                    comps.minute = Some(n);
                }
            }
            "second" => {
                if let Some(n) = tc_get_override_i(v) {
                    comps.second = Some(n);
                }
            }
            "millisecond" => {
                if let Some(n) = tc_get_override_i(v) {
                    // Replace millisecond bits; keep sub-millisecond portion
                    let sub_ms = comps.ns.unwrap_or(0) % 1_000_000;
                    comps.ns = Some(n * 1_000_000 + sub_ms);
                }
            }
            "microsecond" => {
                if let Some(n) = tc_get_override_i(v) {
                    // Replace microsecond bits; keep sub-microsecond (ns % 1000)
                    let ms_bits = (comps.ns.unwrap_or(0) / 1_000_000) * 1_000_000;
                    let sub_us = comps.ns.unwrap_or(0) % 1_000;
                    comps.ns = Some(ms_bits + n * 1_000 + sub_us);
                }
            }
            "nanosecond" => {
                if let Some(n) = tc_get_override_i(v) {
                    // Replace nanosecond bits; keep ms+us portion
                    let upper = (comps.ns.unwrap_or(0) / 1_000) * 1_000;
                    comps.ns = Some(upper + n);
                }
            }
            "timezone" => {
                if let Expression::Literal(Literal::String(s)) = v {
                    comps.tz = Some(tc_tz_suffix(s));
                }
            }
            _ => {}
        }
    }
}

/// Build fractional-second suffix from combined nanoseconds. Empty if zero.
fn tc_fmt_frac(ns: i64) -> String {
    if ns == 0 {
        return String::new();
    }
    let s = format!("{ns:09}");
    format!(".{}", s.trim_end_matches('0'))
}

/// Format a time part "HH:MM" or "HH:MM:SS[.frac]" from components.
pub(crate) fn tc_fmt_time(h: i64, min: i64, sec: i64, ns: i64) -> String {
    let frac = tc_fmt_frac(ns);
    if sec == 0 && ns == 0 {
        format!("{h:02}:{min:02}")
    } else {
        format!("{h:02}:{min:02}:{sec:02}{frac}")
    }
}

// ── Temporal helper functions (pure calendar arithmetic) ─────────────────────

fn temporal_is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

pub(crate) fn temporal_dim(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if temporal_is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// Days since proleptic Gregorian epoch (Jan 1 year 1 = day 1).
pub(crate) fn temporal_epoch(y: i64, m: i64, d: i64) -> i64 {
    let y1 = y - 1;
    let mut n = 365 * y1 + y1 / 4 - y1 / 100 + y1 / 400;
    for mo in 1..m {
        n += temporal_dim(y, mo);
    }
    n + d
}

/// Inverse of temporal_epoch — returns (year, month, day).
fn temporal_from_epoch(mut n: i64) -> (i64, i64, i64) {
    let n400 = (n - 1) / 146097;
    n -= n400 * 146097;
    let n100 = ((n - 1) / 36524).min(3);
    n -= n100 * 36524;
    let n4 = (n - 1) / 1461;
    n -= n4 * 1461;
    let n1 = ((n - 1) / 365).min(3);
    n -= n1 * 365;
    let year = n400 * 400 + n100 * 100 + n4 * 4 + n1 + 1;
    let months = [
        31_i64,
        if temporal_is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1i64;
    let mut rem = n;
    for dm in &months {
        if rem <= *dm {
            break;
        }
        rem -= dm;
        month += 1;
    }
    (year, month, rem)
}

/// ISO week date (iso_year, week 1-53, dow 1=Mon..7=Sun) → "YYYY-MM-DD".
fn temporal_week_to_date(iso_year: i64, week: i64, dow: i64) -> String {
    let jan4 = temporal_epoch(iso_year, 1, 4);
    let jan4_dow = ((jan4 - 1) % 7 + 7) % 7 + 1; // 1=Mon, 7=Sun
    let w1_mon = jan4 - (jan4_dow - 1);
    let target = w1_mon + (week - 1) * 7 + (dow - 1);
    let (y, m, d) = temporal_from_epoch(target);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Ordinal day (1-366) → (month, day) for year y.
fn temporal_ordinal_to_md(y: i64, ord: i64) -> (i64, i64) {
    let months = [
        31_i64,
        if temporal_is_leap(y) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1i64;
    let mut rem = ord;
    for dm in &months {
        if rem <= *dm {
            break;
        }
        rem -= dm;
        month += 1;
    }
    (month, rem)
}

/// Quarter (1-4) + dayOfQuarter (1-92) → (month, day).
fn temporal_quarter_to_md(y: i64, quarter: i64, doq: i64) -> (i64, i64) {
    let start_month = (quarter - 1) * 3 + 1;
    let mut rem = doq;
    let mut month = start_month;
    for _ in 0..3 {
        let dm = temporal_dim(y, month);
        if rem <= dm {
            break;
        }
        rem -= dm;
        month += 1;
    }
    (month, rem)
}

/// Evaluate a numeric expression to i64, handling literals and negation.
fn eval_expr_to_i64(v: &Expression) -> Option<i64> {
    match v {
        Expression::Literal(Literal::Integer(n)) => Some(*n),
        Expression::Literal(Literal::Float(f)) => Some(*f as i64),
        Expression::Negate(inner) => eval_expr_to_i64(inner).map(|n| -n),
        _ => None,
    }
}

/// Evaluate a numeric expression to f64, handling literals and negation.
fn eval_expr_to_f64(v: &Expression) -> Option<f64> {
    match v {
        Expression::Literal(Literal::Float(f)) => Some(*f),
        Expression::Literal(Literal::Integer(n)) => Some(*n as f64),
        Expression::Negate(inner) => eval_expr_to_f64(inner).map(|f| -f),
        _ => None,
    }
}

/// Extract integer value for a case-insensitive key from map pairs.
fn temporal_get_i(pairs: &[(String, Expression)], key: &str) -> Option<i64> {
    pairs.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case(key) {
            eval_expr_to_i64(v)
        } else {
            None
        }
    })
}

/// Extract float value for a case-insensitive key from map pairs.
fn temporal_get_f(pairs: &[(String, Expression)], key: &str) -> Option<f64> {
    pairs.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case(key) {
            eval_expr_to_f64(v)
        } else {
            None
        }
    })
}

/// Extract string value for a case-insensitive key from map pairs.
fn temporal_get_s(pairs: &[(String, Expression)], key: &str) -> Option<String> {
    pairs.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case(key) {
            if let Expression::Literal(Literal::String(s)) = v {
                Some(s.clone())
            } else {
                None
            }
        } else {
            None
        }
    })
}

/// Build fractional-second suffix from millisecond/microsecond/nanosecond fields.
/// Returns "" when no sub-second fields are present or all are zero.
fn temporal_frac(pairs: &[(String, Expression)]) -> String {
    let ms = temporal_get_i(pairs, "millisecond").unwrap_or(0);
    let us = temporal_get_i(pairs, "microsecond").unwrap_or(0);
    let ns = temporal_get_i(pairs, "nanosecond").unwrap_or(0);
    if ms == 0 && us == 0 && ns == 0 {
        return String::new();
    }
    let total = ms * 1_000_000 + us * 1_000 + ns;
    let s = format!("{total:09}");
    format!(".{}", s.trim_end_matches('0'))
}

/// Build fractional-second suffix when `nanosecond` alone is given.
/// Used when only nanosecond is specified (no millisecond/microsecond).
fn temporal_frac_ns_only(pairs: &[(String, Expression)]) -> String {
    match temporal_get_i(pairs, "nanosecond") {
        Some(0) | None => String::new(),
        Some(ns) => {
            let s = format!("{ns:09}");
            format!(".{}", s.trim_end_matches('0'))
        }
    }
}

/// Build fractional second suffix, preferring combined ms/µs/ns over nanosecond-only.
fn temporal_sub_second(pairs: &[(String, Expression)]) -> String {
    let has_ms = temporal_get_i(pairs, "millisecond").is_some();
    let has_us = temporal_get_i(pairs, "microsecond").is_some();
    if has_ms || has_us {
        temporal_frac(pairs)
    } else {
        temporal_frac_ns_only(pairs)
    }
}

/// Compute ISO week components (iso_year, week 1-53, day_of_week 1=Mon..7=Sun)
/// from a calendar date (y, m, d).
pub(crate) fn date_to_iso_week(y: i64, m: i64, d: i64) -> (i64, i64, i64) {
    let epoch = temporal_epoch(y, m, d);
    let dow = ((epoch - 1) % 7 + 7) % 7 + 1; // 1=Mon, 7=Sun
                                             // Thursday of the current ISO week
    let thu_epoch = epoch - dow + 4;
    let (thu_y, _, _) = temporal_from_epoch(thu_epoch);
    let iso_year = thu_y;
    let jan4_of_iso = temporal_epoch(iso_year, 1, 4);
    let jan4_dow = ((jan4_of_iso - 1) % 7 + 7) % 7 + 1;
    let w1_mon = jan4_of_iso - (jan4_dow - 1);
    let week = (epoch - w1_mon) / 7 + 1;
    (iso_year, week, dow)
}

/// Extract compile-time (year, month, day) triple from a `date(...)` expression
/// or from a string literal that represents a temporal value.
fn extract_base_date_ymd(v: &Expression) -> Option<(i64, i64, i64)> {
    match v {
        Expression::FunctionCall { name, args, .. } if name.eq_ignore_ascii_case("date") => {
            if let Some(arg) = args.first() {
                let s = match arg {
                    Expression::Literal(Literal::String(s)) => temporal_parse_date(s)?,
                    Expression::Map(inner) => temporal_date_from_map(inner)?,
                    _ => return None,
                };
                // Parse YYYY-MM-DD
                let parts: Vec<&str> = s.splitn(3, '-').collect();
                if parts.len() == 3 {
                    let y: i64 = parts[0].parse().ok()?;
                    let m: i64 = parts[1].parse().ok()?;
                    let d: i64 = parts[2].parse().ok()?;
                    return Some((y, m, d));
                }
            }
            None
        }
        Expression::Literal(Literal::String(s)) => {
            // String from with_lit_vars (date, localdatetime, or datetime).
            // Strip time part if present, then parse the date.
            let ds = temporal_parse_date(s)?;
            let parts: Vec<&str> = ds.splitn(3, '-').collect();
            if parts.len() == 3 {
                let y: i64 = parts[0].parse().ok()?;
                let m: i64 = parts[1].parse().ok()?;
                let d: i64 = parts[2].parse().ok()?;
                Some((y, m, d))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Construct a `date` literal from a map.  Returns `None` if the map is
/// incomplete or contains runtime-variable references (e.g. `date: otherVar`).
pub(crate) fn temporal_date_from_map(pairs: &[(String, Expression)]) -> Option<String> {
    // Check for a `date` key providing a base date for week-based construction.
    let base_ymd: Option<(i64, i64, i64)> = pairs.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case("date") {
            extract_base_date_ymd(v)
        } else {
            None
        }
    });

    if let Some((by, bm, bd)) = base_ymd {
        // Derive ALL components from the base date for defaults.
        let (base_iso_year, base_week, base_dow) = date_to_iso_week(by, bm, bd);
        // Ordinal day within the year.
        let base_ord = temporal_epoch(by, bm, bd) - temporal_epoch(by, 1, 1) + 1;
        // Quarter and day-of-quarter.
        let base_q = (bm - 1) / 3 + 1;
        let base_doq: i64 = {
            let qs = (base_q - 1) * 3 + 1;
            let mut doq = bd;
            for mo in qs..bm {
                doq += temporal_dim(by, mo);
            }
            doq
        };

        // Dispatch on which override key(s) are present.
        if temporal_get_i(pairs, "week").is_some() || temporal_get_i(pairs, "dayOfWeek").is_some() {
            let iso_year = temporal_get_i(pairs, "year").unwrap_or(base_iso_year);
            let week = temporal_get_i(pairs, "week").unwrap_or(base_week);
            let dow = temporal_get_i(pairs, "dayOfWeek").unwrap_or(base_dow);
            return Some(temporal_week_to_date(iso_year, week, dow));
        } else if let Some(ord) = temporal_get_i(pairs, "ordinalDay") {
            let year = temporal_get_i(pairs, "year").unwrap_or(by);
            let (m, d) = temporal_ordinal_to_md(year, ord);
            return Some(format!("{year:04}-{m:02}-{d:02}"));
        } else if temporal_get_i(pairs, "quarter").is_some()
            || temporal_get_i(pairs, "dayOfQuarter").is_some()
        {
            let year = temporal_get_i(pairs, "year").unwrap_or(by);
            let q = temporal_get_i(pairs, "quarter").unwrap_or(base_q);
            let doq = temporal_get_i(pairs, "dayOfQuarter").unwrap_or(base_doq);
            let (m, d) = temporal_quarter_to_md(year, q, doq);
            return Some(format!("{year:04}-{m:02}-{d:02}"));
        } else {
            // Calendar date (year/month/day overrides).
            let year = temporal_get_i(pairs, "year").unwrap_or(by);
            let month = temporal_get_i(pairs, "month").unwrap_or(bm);
            let day = temporal_get_i(pairs, "day").unwrap_or(bd);
            // Avoid unused variable warnings.
            let _ = base_ord;
            return Some(format!("{year:04}-{month:02}-{day:02}"));
        }
    }

    let year = temporal_get_i(pairs, "year")?;
    if let Some(m) = temporal_get_i(pairs, "month") {
        let d = temporal_get_i(pairs, "day").unwrap_or(1);
        return Some(format!("{year:04}-{m:02}-{d:02}"));
    }
    if let Some(w) = temporal_get_i(pairs, "week") {
        let dow = temporal_get_i(pairs, "dayOfWeek").unwrap_or(1);
        return Some(temporal_week_to_date(year, w, dow));
    }
    if let Some(ord) = temporal_get_i(pairs, "ordinalDay") {
        let (m, d) = temporal_ordinal_to_md(year, ord);
        return Some(format!("{year:04}-{m:02}-{d:02}"));
    }
    if let Some(q) = temporal_get_i(pairs, "quarter") {
        let doq = temporal_get_i(pairs, "dayOfQuarter").unwrap_or(1);
        let (m, d) = temporal_quarter_to_md(year, q, doq);
        return Some(format!("{year:04}-{m:02}-{d:02}"));
    }
    // Year only → first of year
    Some(format!("{year:04}-01-01"))
}

/// Extract a base time string from an expression value (for `time`/`localtime` map keys).
/// Returns `temporal_parse_localtime` result (no TZ).
fn extract_base_time_str(v: &Expression) -> Option<String> {
    match v {
        Expression::FunctionCall { name, args, .. }
            if name.eq_ignore_ascii_case("localtime")
                || name.eq_ignore_ascii_case("time")
                || name.eq_ignore_ascii_case("localdatetime")
                || name.eq_ignore_ascii_case("datetime") =>
        {
            if let Some(arg) = args.first() {
                match arg {
                    Expression::Literal(Literal::String(s)) => temporal_parse_localtime(s),
                    Expression::Map(p) => temporal_localtime_from_map(p),
                    _ => None,
                }
            } else {
                None
            }
        }
        Expression::Literal(Literal::String(s)) => temporal_parse_localtime(s),
        _ => None,
    }
}

/// Extract time string with timezone from a temporal expression, for use in datetime construction.
/// Returns (time_string, original_had_tz):
/// - time_string: full time string (including TZ if present, omitting date part for datetime)
/// - original_had_tz: true if the source expression carried an explicit timezone
fn extract_base_time_with_tz(v: &Expression) -> Option<(String, bool)> {
    match v {
        Expression::FunctionCall { name, args, .. } => {
            let ls = name.to_lowercase();
            let arg = args.first()?;
            match (ls.as_str(), arg) {
                ("time", Expression::Literal(Literal::String(s))) => {
                    temporal_parse_time(s).map(|t| (t, true))
                }
                ("time", Expression::Map(p)) => temporal_time_from_map(p).map(|t| (t, true)),
                ("localtime", Expression::Literal(Literal::String(s))) => {
                    temporal_parse_localtime(s).map(|t| (t, false))
                }
                ("localtime", Expression::Map(p)) => {
                    temporal_localtime_from_map(p).map(|t| (t, false))
                }
                ("datetime", Expression::Literal(Literal::String(s))) => {
                    let dt = temporal_parse_datetime(s)?;
                    let t_pos = dt.find('T')?;
                    Some((dt[t_pos + 1..].to_owned(), true))
                }
                ("datetime", Expression::Map(p)) => {
                    let dt = temporal_datetime_from_map(p)?;
                    let t_pos = dt.find('T')?;
                    Some((dt[t_pos + 1..].to_owned(), true))
                }
                ("localdatetime", Expression::Literal(Literal::String(s))) => {
                    let dt = temporal_parse_localdatetime(s)?;
                    let t_pos = dt.find('T')?;
                    Some((dt[t_pos + 1..].to_owned(), false))
                }
                ("localdatetime", Expression::Map(p)) => {
                    let dt = temporal_localdatetime_from_map(p)?;
                    let t_pos = dt.find('T')?;
                    Some((dt[t_pos + 1..].to_owned(), false))
                }
                _ => None,
            }
        }
        Expression::Literal(Literal::String(s)) => {
            // Detect if s has an explicit timezone suffix.
            let has_tz = s.ends_with('Z')
                || s.contains('+')
                || s.rfind('-').map_or(false, |p| {
                    p > 8
                        && s.as_bytes()
                            .get(p + 1)
                            .map_or(false, |b| b.is_ascii_digit())
                });
            if has_tz {
                temporal_parse_time(s).map(|t| (t, true))
            } else {
                temporal_parse_localtime(s).map(|t| (t, false))
            }
        }
        _ => None,
    }
}

/// Parse a localtime string "HH:MM[:SS[.frac]]" into (h, min, sec, sub_sec_ns).
/// Returns None if the string cannot be parsed.
fn parse_localtime_to_parts(s: &str) -> Option<(i64, i64, i64, i64)> {
    if s.len() < 5 || s.as_bytes().get(2) != Some(&b':') {
        return None;
    }
    let h: i64 = s[..2].parse().ok()?;
    let min: i64 = s[3..5].parse().ok()?;
    if s.len() == 5 {
        return Some((h, min, 0, 0));
    }
    if s.as_bytes().get(5) != Some(&b':') {
        return Some((h, min, 0, 0));
    }
    let sec_rest = &s[6..];
    if let Some(dot) = sec_rest.find('.') {
        let sec: i64 = sec_rest[..dot].parse().ok()?;
        let frac_str = &sec_rest[dot + 1..];
        // Pad/truncate to 9 digits for nanoseconds
        let padded = format!("{:0<9}", &frac_str[..frac_str.len().min(9)]);
        let ns: i64 = padded.parse().ok()?;
        Some((h, min, sec, ns))
    } else {
        let sec: i64 = sec_rest.parse().ok()?;
        Some((h, min, sec, 0))
    }
}

/// Reconstruct a localtime string "HH:MM[:SS[.frac]]" from parts.
fn localtime_parts_to_str(h: i64, min: i64, sec: i64, ns: i64) -> String {
    if sec == 0 && ns == 0 {
        return format!("{h:02}:{min:02}");
    }
    let frac = if ns == 0 {
        String::new()
    } else {
        let frac_str = format!("{ns:09}");
        format!(".{}", frac_str.trim_end_matches('0'))
    };
    format!("{h:02}:{min:02}:{sec:02}{frac}")
}

/// Parse a normalized TZ string like "+01:00" or "Z" to seconds.
fn parse_tz_offset_s(tz: &str) -> Option<i64> {
    if tz == "Z" || tz.is_empty() {
        return Some(0);
    }
    if tz.starts_with('+') || tz.starts_with('-') {
        let sign: i64 = if tz.starts_with('-') { -1 } else { 1 };
        let rest = &tz[1..];
        // Strip bracket suffix if present
        let rest = if let Some(b) = rest.find('[') {
            &rest[..b]
        } else {
            rest
        };
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        if parts.len() == 2 {
            let h: i64 = parts[0].parse().ok()?;
            let m: i64 = parts[1].parse().ok()?;
            return Some(sign * (h * 3600 + m * 60));
        }
    }
    None
}

pub(crate) fn temporal_localtime_from_map(pairs: &[(String, Expression)]) -> Option<String> {
    // Check for a `time` key providing base time components.
    let base_time: Option<(i64, i64, i64, i64)> = pairs.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case("time") {
            let ts = extract_base_time_str(v)?;
            parse_localtime_to_parts(&ts)
        } else {
            None
        }
    });

    if let Some((bh, bmin, bsec, bns)) = base_time {
        // Apply overrides over the base.
        let h = temporal_get_i(pairs, "hour").unwrap_or(bh);
        let min = temporal_get_i(pairs, "minute").unwrap_or(bmin);
        let sec = temporal_get_i(pairs, "second").unwrap_or(bsec);
        // Sub-second override: if any override is specified use those, else use base ns.
        let ns = if temporal_get_i(pairs, "millisecond").is_some()
            || temporal_get_i(pairs, "microsecond").is_some()
            || temporal_get_i(pairs, "nanosecond").is_some()
        {
            let ms = temporal_get_i(pairs, "millisecond").unwrap_or(0);
            let us = temporal_get_i(pairs, "microsecond").unwrap_or(0);
            let ns_v = temporal_get_i(pairs, "nanosecond").unwrap_or(0);
            ms * 1_000_000 + us * 1_000 + ns_v
        } else {
            bns
        };
        return Some(localtime_parts_to_str(h, min, sec, ns));
    }

    // Original logic: require hour.
    let h = temporal_get_i(pairs, "hour")?;
    let min = temporal_get_i(pairs, "minute").unwrap_or(0);
    let sec = temporal_get_i(pairs, "second");
    let frac = temporal_sub_second(pairs);
    // Always include seconds to produce a valid xsd:time value ("HH:MM:SS[.frac]").
    // Strip_zero_seconds_from_time in the TCK runner will canonicalize back to
    // "HH:MM" when seconds are zero and there is no fractional part.
    match sec {
        None if frac.is_empty() => Some(format!("{h:02}:{min:02}:00")),
        None => Some(format!("{h:02}:{min:02}:00{frac}")),
        Some(s) => Some(format!("{h:02}:{min:02}:{s:02}{frac}")),
    }
}

/// Construct a `time` literal from a map.
pub(crate) fn temporal_time_from_map(pairs: &[(String, Expression)]) -> Option<String> {
    // Check for a `time` key providing a base time.
    // Returns (time_string, original_had_tz): the bool indicates if the source had a TZ.
    // Local times (no TZ) should NOT be converted when a new timezone is specified;
    // they just get the TZ attached.  Times with an explicit TZ ARE converted via UTC.
    let base_time_raw: Option<(String, bool)> = pairs.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case("time") {
            // Get the raw time string (with potential TZ from original).
            match v {
                Expression::FunctionCall { name, args, .. } => {
                    let ls = name.to_lowercase();
                    let base = ls.as_str();
                    if let Some(arg) = args.first() {
                        match arg {
                            Expression::Literal(Literal::String(s)) => {
                                if base == "time" {
                                    // time() function always has TZ.
                                    temporal_parse_time(s).map(|t| (t, true))
                                } else {
                                    // localtime() / localdatetime() / etc.: no TZ.
                                    temporal_parse_localtime(s).map(|lt| (lt, false))
                                }
                            }
                            Expression::Map(p) => {
                                if base == "time" {
                                    temporal_time_from_map(p).map(|t| (t, true))
                                } else {
                                    temporal_localtime_from_map(p).map(|lt| (lt, false))
                                }
                            }
                            _ => None,
                        }
                    } else {
                        None
                    }
                }
                Expression::Literal(Literal::String(s)) => {
                    // Check if the string already has a timezone suffix.
                    let (_, _raw_tz) = split_tz(s);
                    // split_tz returns "Z" as default for strings with no timezone;
                    // but "Z" could also be explicit. Detect explicit TZ by checking
                    // whether the original string ends with 'Z' or has +/-.
                    let has_tz = s.ends_with('Z')
                        || s.contains('+')
                        || s.rfind('-').map_or(false, |p| {
                            // '-' after position 8 is likely TZ sign, not date separator
                            p > 8
                                && s.as_bytes()
                                    .get(p + 1)
                                    .map_or(false, |b| b.is_ascii_digit())
                        });
                    if has_tz {
                        temporal_parse_time(s).map(|t| (t, true))
                    } else {
                        // Local time or local datetime string: keep as localtime, no UTC.
                        temporal_parse_localtime(s).map(|lt| (lt, false))
                    }
                }
                _ => None,
            }
        } else {
            None
        }
    });

    if let Some((base_str, orig_had_tz)) = base_time_raw {
        // Extract components from base_str.
        // For local times (orig_had_tz=false), base_str has no TZ suffix.
        // For timestamped times (orig_had_tz=true), base_str has TZ suffix.
        let (time_body, tz_raw_base) = if orig_had_tz {
            split_tz(&base_str)
        } else {
            // Local time: body is the full string, no TZ.
            (base_str.as_str(), "")
        };
        let base_parts = parse_localtime_to_parts(time_body)?;
        let (bh, bmin, bsec, bns) = base_parts;
        let base_tz_s = if tz_raw_base.is_empty() {
            0
        } else {
            parse_tz_offset_s(&normalize_tz(tz_raw_base)).unwrap_or(0)
        };

        // Apply overrides.
        let override_h = temporal_get_i(pairs, "hour");
        let override_min = temporal_get_i(pairs, "minute");
        let override_sec = temporal_get_i(pairs, "second");
        let override_tz = temporal_get_s(pairs, "timezone");
        let has_override_subsec = temporal_get_i(pairs, "millisecond").is_some()
            || temporal_get_i(pairs, "microsecond").is_some()
            || temporal_get_i(pairs, "nanosecond").is_some();

        let new_tz_str = override_tz.as_deref().map(tc_tz_suffix);
        let new_tz_s = new_tz_str
            .as_deref()
            .and_then(|tz| parse_tz_offset_s(tz))
            .unwrap_or(base_tz_s);
        // For time() values, named timezone brackets are stripped; only numeric offsets remain.
        let tz_str = new_tz_str.unwrap_or_else(|| {
            if tz_raw_base.is_empty() {
                "Z".to_owned()
            } else {
                strip_named_tz(&normalize_tz(tz_raw_base))
            }
        });

        // Compute wall-clock time: if TZ changed AND base has a known TZ, convert UTC then apply new TZ.
        // If base is a local time (no TZ), just attach the new TZ without conversion.
        let (h, min, sec, ns) =
            if new_tz_s != base_tz_s && override_tz.is_some() && !tz_raw_base.is_empty() {
                // Convert wall clock to UTC then to new TZ.
                let base_wall_s = bh * 3600 + bmin * 60 + bsec;
                let utc_s = base_wall_s - base_tz_s;
                let new_wall_s = utc_s + new_tz_s;
                let new_h = ((new_wall_s / 3600) % 24 + 24) % 24;
                let new_min = (new_wall_s % 3600) / 60;
                let new_sec_v = new_wall_s % 60;
                (
                    override_h.unwrap_or(new_h),
                    override_min.unwrap_or(new_min.abs()),
                    override_sec.unwrap_or(new_sec_v.abs()),
                    if has_override_subsec {
                        let ms = temporal_get_i(pairs, "millisecond").unwrap_or(0);
                        let us = temporal_get_i(pairs, "microsecond").unwrap_or(0);
                        let ns_v = temporal_get_i(pairs, "nanosecond").unwrap_or(0);
                        ms * 1_000_000 + us * 1_000 + ns_v
                    } else {
                        bns
                    },
                )
            } else {
                (
                    override_h.unwrap_or(bh),
                    override_min.unwrap_or(bmin),
                    override_sec.unwrap_or(bsec),
                    if has_override_subsec {
                        let ms = temporal_get_i(pairs, "millisecond").unwrap_or(0);
                        let us = temporal_get_i(pairs, "microsecond").unwrap_or(0);
                        let ns_v = temporal_get_i(pairs, "nanosecond").unwrap_or(0);
                        ms * 1_000_000 + us * 1_000 + ns_v
                    } else {
                        bns
                    },
                )
            };

        let time_s = localtime_parts_to_str(h, min, sec, ns);
        return Some(format!("{time_s}{tz_str}"));
    }

    let h = temporal_get_i(pairs, "hour")?;
    let min = temporal_get_i(pairs, "minute").unwrap_or(0);
    let tz = temporal_get_s(pairs, "timezone")
        .map(|s| tc_tz_suffix(&s))
        .unwrap_or_else(|| "Z".to_string());
    let sec = temporal_get_i(pairs, "second");
    let frac = temporal_sub_second(pairs);
    match sec {
        // Always include :00 seconds for TZ-aware times so that xsd:time literals
        // are valid XSD time format (Oxigraph requires seconds for comparison).
        // The TCK runner's strip_zero_seconds_from_time() restores the display form.
        None if frac.is_empty() => Some(format!("{h:02}:{min:02}:00{tz}")),
        None => Some(format!("{h:02}:{min:02}:00{frac}{tz}")),
        Some(s) => Some(format!("{h:02}:{min:02}:{s:02}{frac}{tz}")),
    }
}

/// Construct a `localdatetime` literal from a map.
pub(crate) fn temporal_localdatetime_from_map(pairs: &[(String, Expression)]) -> Option<String> {
    // Check for a `datetime` key providing both date+time from an existing datetime/localdatetime.
    if let Some(dt_expr) = pairs.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case("datetime") {
            Some(v)
        } else {
            None
        }
    }) {
        if let Expression::Literal(Literal::String(dt_str)) = dt_expr {
            let t_pos = dt_str.find('T')?;
            let date_str = &dt_str[..t_pos];
            let time_part_raw = &dt_str[t_pos + 1..];
            // Strip TZ for localdatetime (ignore any timezone in source)
            let (time_body, _tz) = split_tz(time_part_raw);
            // Parse date parts from base
            let date_parsed = temporal_parse_date(date_str)?;
            let dp: Vec<&str> = date_parsed.splitn(3, '-').collect();
            let (by, bm, bd): (i64, i64, i64) = (
                dp[0].parse().ok()?,
                dp[1].parse().ok()?,
                dp[2].parse().ok()?,
            );
            let (bh, bmin, bsec, bns) = parse_localtime_to_parts(time_body).unwrap_or((0, 0, 0, 0));
            // Apply individual overrides
            let year = temporal_get_i(pairs, "year").unwrap_or(by);
            let month = temporal_get_i(pairs, "month").unwrap_or(bm);
            let day = temporal_get_i(pairs, "day").unwrap_or(bd);
            let h = temporal_get_i(pairs, "hour").unwrap_or(bh);
            let min = temporal_get_i(pairs, "minute").unwrap_or(bmin);
            let sec = temporal_get_i(pairs, "second").unwrap_or(bsec);
            let ns = if temporal_get_i(pairs, "millisecond").is_some()
                || temporal_get_i(pairs, "microsecond").is_some()
                || temporal_get_i(pairs, "nanosecond").is_some()
            {
                let ms = temporal_get_i(pairs, "millisecond").unwrap_or(0);
                let us = temporal_get_i(pairs, "microsecond").unwrap_or(0);
                let ns_v = temporal_get_i(pairs, "nanosecond").unwrap_or(0);
                ms * 1_000_000 + us * 1_000 + ns_v
            } else {
                bns
            };
            let date_out = format!("{year:04}-{month:02}-{day:02}");
            let time_out = localtime_parts_to_str(h, min, sec, ns);
            return Some(format!("{date_out}T{time_out}"));
        }
    }

    // Check for a `date` key as base for week-based construction.
    let base_ymd: Option<(i64, i64, i64)> = pairs.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case("date") {
            extract_base_date_ymd(v)
        } else {
            None
        }
    });
    // Check for a `time` key as base for time components.
    let base_time_parts: Option<(i64, i64, i64, i64)> = pairs.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case("time") {
            let ts = extract_base_time_str(v)?;
            parse_localtime_to_parts(&ts)
        } else {
            None
        }
    });

    let date_part = if let Some((by, bm, bd)) = base_ymd {
        let (base_iso_year, base_week, base_dow) = date_to_iso_week(by, bm, bd);
        // Apply date overrides on top of base.
        let has_week = temporal_get_i(pairs, "week").is_some();
        let has_ord = temporal_get_i(pairs, "ordinalDay").is_some();
        let has_q = temporal_get_i(pairs, "quarter").is_some();
        if has_week {
            let iso_year = temporal_get_i(pairs, "year").unwrap_or(base_iso_year);
            let week = temporal_get_i(pairs, "week").unwrap_or(base_week);
            let dow = temporal_get_i(pairs, "dayOfWeek").unwrap_or(base_dow);
            temporal_week_to_date(iso_year, week, dow)
        } else if has_ord {
            let year = temporal_get_i(pairs, "year").unwrap_or(by);
            let ord = temporal_get_i(pairs, "ordinalDay").unwrap();
            let (m, d) = temporal_ordinal_to_md(year, ord);
            format!("{year:04}-{m:02}-{d:02}")
        } else if has_q {
            let year = temporal_get_i(pairs, "year").unwrap_or(by);
            let q = temporal_get_i(pairs, "quarter").unwrap();
            let doq = temporal_get_i(pairs, "dayOfQuarter").unwrap_or(1);
            let (m, d) = temporal_quarter_to_md(year, q, doq);
            format!("{year:04}-{m:02}-{d:02}")
        } else {
            let year = temporal_get_i(pairs, "year").unwrap_or(by);
            let m = temporal_get_i(pairs, "month").unwrap_or(bm);
            let d = temporal_get_i(pairs, "day").unwrap_or(bd);
            format!("{year:04}-{m:02}-{d:02}")
        }
    } else {
        let year = temporal_get_i(pairs, "year")?;
        if let Some(m) = temporal_get_i(pairs, "month") {
            let d = temporal_get_i(pairs, "day").unwrap_or(1);
            format!("{year:04}-{m:02}-{d:02}")
        } else if let Some(w) = temporal_get_i(pairs, "week") {
            let dow = temporal_get_i(pairs, "dayOfWeek").unwrap_or(1);
            temporal_week_to_date(year, w, dow)
        } else if let Some(ord) = temporal_get_i(pairs, "ordinalDay") {
            let (m, d) = temporal_ordinal_to_md(year, ord);
            format!("{year:04}-{m:02}-{d:02}")
        } else if let Some(q) = temporal_get_i(pairs, "quarter") {
            let doq = temporal_get_i(pairs, "dayOfQuarter").unwrap_or(1);
            let (m, d) = temporal_quarter_to_md(year, q, doq);
            format!("{year:04}-{m:02}-{d:02}")
        } else {
            format!("{year:04}-01-01")
        }
    };

    if let Some((bh, bmin, bsec, bns)) = base_time_parts {
        let h = temporal_get_i(pairs, "hour").unwrap_or(bh);
        let min = temporal_get_i(pairs, "minute").unwrap_or(bmin);
        let sec = temporal_get_i(pairs, "second").unwrap_or(bsec);
        let ns = if temporal_get_i(pairs, "millisecond").is_some()
            || temporal_get_i(pairs, "microsecond").is_some()
            || temporal_get_i(pairs, "nanosecond").is_some()
        {
            let ms = temporal_get_i(pairs, "millisecond").unwrap_or(0);
            let us = temporal_get_i(pairs, "microsecond").unwrap_or(0);
            let ns_v = temporal_get_i(pairs, "nanosecond").unwrap_or(0);
            ms * 1_000_000 + us * 1_000 + ns_v
        } else {
            bns
        };
        let time_part = localtime_parts_to_str(h, min, sec, ns);
        return Some(format!("{date_part}T{time_part}"));
    }

    let h = temporal_get_i(pairs, "hour").unwrap_or(0);
    let min = temporal_get_i(pairs, "minute").unwrap_or(0);
    let sec = temporal_get_i(pairs, "second");
    let frac = temporal_sub_second(pairs);
    let time_part = match sec {
        None if frac.is_empty() => format!("{h:02}:{min:02}"),
        None => format!("{h:02}:{min:02}:00{frac}"),
        Some(s) => format!("{h:02}:{min:02}:{s:02}{frac}"),
    };
    Some(format!("{date_part}T{time_part}"))
}

/// Construct a `datetime` literal from a map.
pub(crate) fn temporal_datetime_from_map(pairs: &[(String, Expression)]) -> Option<String> {
    // Check for a `datetime` key providing both date+time from an existing datetime/localdatetime.
    if let Some(dt_expr) = pairs.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case("datetime") {
            Some(v)
        } else {
            None
        }
    }) {
        if let Expression::Literal(Literal::String(dt_str)) = dt_expr {
            let t_pos = dt_str.find('T')?;
            let date_str = &dt_str[..t_pos];
            let time_part_raw = &dt_str[t_pos + 1..];
            // Parse base TZ from the source datetime
            let (time_body, base_tz_raw) = split_tz(time_part_raw);
            let orig_had_tz = !base_tz_raw.is_empty() && base_tz_raw != "Z";
            let base_tz_s = parse_tz_offset_s(&normalize_tz(base_tz_raw)).unwrap_or(0);
            // Parse date parts from base
            let date_parsed = temporal_parse_date(date_str)?;
            let dp: Vec<&str> = date_parsed.splitn(3, '-').collect();
            let (by, bm, bd): (i64, i64, i64) = (
                dp[0].parse().ok()?,
                dp[1].parse().ok()?,
                dp[2].parse().ok()?,
            );
            let (bh, bmin, bsec, bns) = parse_localtime_to_parts(time_body).unwrap_or((0, 0, 0, 0));
            // Apply date overrides
            let year = temporal_get_i(pairs, "year").unwrap_or(by);
            let month = temporal_get_i(pairs, "month").unwrap_or(bm);
            let day = temporal_get_i(pairs, "day").unwrap_or(bd);
            let year = temporal_get_i(pairs, "year").unwrap_or(by);
            let override_tz_str =
                temporal_get_s(pairs, "timezone").map(|s| tc_tz_suffix_ymd(&s, year, month, day));
            let new_tz_s = override_tz_str
                .as_deref()
                .and_then(parse_tz_offset_s)
                .unwrap_or(base_tz_s);
            let tz = override_tz_str.clone().unwrap_or_else(|| {
                let n = normalize_tz(base_tz_raw);
                if n.is_empty() {
                    "Z".to_owned()
                } else {
                    n
                }
            });
            // UTC conversion when TZ changes
            let (h, min, sec, ns) =
                if orig_had_tz && override_tz_str.is_some() && new_tz_s != base_tz_s {
                    let base_wall_s = bh * 3600 + bmin * 60 + bsec;
                    let utc_s = base_wall_s - base_tz_s;
                    let new_wall_s = utc_s + new_tz_s;
                    let new_h = ((new_wall_s / 3600) % 24 + 24) % 24;
                    let new_min = ((new_wall_s % 3600) / 60 + 60) % 60;
                    let new_sec = (new_wall_s % 60 + 60) % 60;
                    (
                        temporal_get_i(pairs, "hour").unwrap_or(new_h),
                        temporal_get_i(pairs, "minute").unwrap_or(new_min),
                        temporal_get_i(pairs, "second").unwrap_or(new_sec),
                        if temporal_get_i(pairs, "millisecond").is_some()
                            || temporal_get_i(pairs, "microsecond").is_some()
                            || temporal_get_i(pairs, "nanosecond").is_some()
                        {
                            let ms = temporal_get_i(pairs, "millisecond").unwrap_or(0);
                            let us = temporal_get_i(pairs, "microsecond").unwrap_or(0);
                            let ns_v = temporal_get_i(pairs, "nanosecond").unwrap_or(0);
                            ms * 1_000_000 + us * 1_000 + ns_v
                        } else {
                            bns
                        },
                    )
                } else {
                    (
                        temporal_get_i(pairs, "hour").unwrap_or(bh),
                        temporal_get_i(pairs, "minute").unwrap_or(bmin),
                        temporal_get_i(pairs, "second").unwrap_or(bsec),
                        if temporal_get_i(pairs, "millisecond").is_some()
                            || temporal_get_i(pairs, "microsecond").is_some()
                            || temporal_get_i(pairs, "nanosecond").is_some()
                        {
                            let ms = temporal_get_i(pairs, "millisecond").unwrap_or(0);
                            let us = temporal_get_i(pairs, "microsecond").unwrap_or(0);
                            let ns_v = temporal_get_i(pairs, "nanosecond").unwrap_or(0);
                            ms * 1_000_000 + us * 1_000 + ns_v
                        } else {
                            bns
                        },
                    )
                };
            let date_out = format!("{year:04}-{month:02}-{day:02}");
            let time_out = localtime_parts_to_str(h, min, sec, ns);
            return Some(format!("{date_out}T{time_out}{tz}"));
        }
    }

    // Check for a `date` key as base for week-based construction.
    let base_ymd: Option<(i64, i64, i64)> = pairs.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case("date") {
            extract_base_date_ymd(v)
        } else {
            None
        }
    });
    // Check for a `time` key as base for time components, preserving its timezone.
    let base_time_data: Option<(i64, i64, i64, i64, String, bool)> =
        pairs.iter().find_map(|(k, v)| {
            if !k.eq_ignore_ascii_case("time") {
                return None;
            }
            let (time_str, orig_had_tz) = extract_base_time_with_tz(v)?;
            let (body, tz_raw) = if orig_had_tz {
                split_tz_owned(&time_str)
            } else {
                (time_str.clone(), String::new())
            };
            let parts = parse_localtime_to_parts(&body)?;
            Some((parts.0, parts.1, parts.2, parts.3, tz_raw, orig_had_tz))
        });
    // Extract date_part and month (for DST timezone computation).
    let (month, date_part) = if let Some((by, bm, bd)) = base_ymd {
        let (base_iso_year, base_week, base_dow) = date_to_iso_week(by, bm, bd);
        let has_week = temporal_get_i(pairs, "week").is_some();
        let has_ord = temporal_get_i(pairs, "ordinalDay").is_some();
        let has_q = temporal_get_i(pairs, "quarter").is_some();
        if has_week {
            let iso_year = temporal_get_i(pairs, "year").unwrap_or(base_iso_year);
            let week = temporal_get_i(pairs, "week").unwrap_or(base_week);
            let dow = temporal_get_i(pairs, "dayOfWeek").unwrap_or(base_dow);
            let ds = temporal_week_to_date(iso_year, week, dow);
            let m: i64 = ds[5..7].parse().unwrap_or(1);
            (m, ds)
        } else if has_ord {
            let year = temporal_get_i(pairs, "year").unwrap_or(by);
            let ord = temporal_get_i(pairs, "ordinalDay").unwrap();
            let (m, d) = temporal_ordinal_to_md(year, ord);
            (m, format!("{year:04}-{m:02}-{d:02}"))
        } else if has_q {
            let year = temporal_get_i(pairs, "year").unwrap_or(by);
            let q = temporal_get_i(pairs, "quarter").unwrap();
            let doq = temporal_get_i(pairs, "dayOfQuarter").unwrap_or(1);
            let (m, d) = temporal_quarter_to_md(year, q, doq);
            (m, format!("{year:04}-{m:02}-{d:02}"))
        } else {
            let year = temporal_get_i(pairs, "year").unwrap_or(by);
            let m = temporal_get_i(pairs, "month").unwrap_or(bm);
            let d = temporal_get_i(pairs, "day").unwrap_or(bd);
            (m, format!("{year:04}-{m:02}-{d:02}"))
        }
    } else {
        let year = temporal_get_i(pairs, "year")?;
        if let Some(m) = temporal_get_i(pairs, "month") {
            let d = temporal_get_i(pairs, "day").unwrap_or(1);
            (m, format!("{year:04}-{m:02}-{d:02}"))
        } else if let Some(w) = temporal_get_i(pairs, "week") {
            let dow = temporal_get_i(pairs, "dayOfWeek").unwrap_or(1);
            let ds = temporal_week_to_date(year, w, dow);
            let m: i64 = ds[5..7].parse().unwrap_or(1);
            (m, ds)
        } else if let Some(ord) = temporal_get_i(pairs, "ordinalDay") {
            let (m, d) = temporal_ordinal_to_md(year, ord);
            (m, format!("{year:04}-{m:02}-{d:02}"))
        } else if let Some(q) = temporal_get_i(pairs, "quarter") {
            let doq = temporal_get_i(pairs, "dayOfQuarter").unwrap_or(1);
            let (m, d) = temporal_quarter_to_md(year, q, doq);
            (m, format!("{year:04}-{m:02}-{d:02}"))
        } else {
            (1, format!("{year:04}-01-01"))
        }
    };
    let override_tz_str = temporal_get_s(pairs, "timezone").map(|s| {
        // Use precise DST calculation from the full date (year, month, day) and
        // the hour (if provided), so that the DST boundary on the transition day
        // is resolved correctly (e.g. Oct 29 00:00 = summer, 04:00 = winter).
        let dp: Vec<&str> = date_part.splitn(3, '-').collect();
        let y: i64 = dp.get(0).and_then(|s| s.parse().ok()).unwrap_or(0);
        let m: i64 = dp.get(1).and_then(|s| s.parse().ok()).unwrap_or(month);
        let d: i64 = dp.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
        // Peek at the hour override (if any) for hour-aware DST.  Use noon
        // as the default so that non-transition days are unaffected.
        let h: i64 = temporal_get_i(pairs, "hour").unwrap_or(12);
        tc_tz_suffix_ymdh(&s, y, m, d, h)
    });
    if let Some((bh, bmin, bsec, bns, ref base_tz_raw, orig_had_tz)) = base_time_data {
        let override_h = temporal_get_i(pairs, "hour");
        let override_min = temporal_get_i(pairs, "minute");
        let override_sec = temporal_get_i(pairs, "second");
        let has_override_subsec = temporal_get_i(pairs, "millisecond").is_some()
            || temporal_get_i(pairs, "microsecond").is_some()
            || temporal_get_i(pairs, "nanosecond").is_some();
        let base_tz_s = if base_tz_raw.is_empty() {
            0
        } else {
            // When base_tz_raw has a named timezone (e.g. "+01:00[Europe/Stockholm]"),
            // compute the DST offset for the FINAL date (not the source date) so that
            // "12:00 in Stockholm on March 28" correctly converts to UTC using the
            // March offset (+02:00), not the source date's offset (+01:00).
            let norm = normalize_tz(base_tz_raw);
            let effective_tz = if let Some(brk) = norm.find('[') {
                let tz_name = &norm[brk + 1..norm.len().saturating_sub(1)];
                let dp: Vec<&str> = date_part.splitn(3, '-').collect();
                let y: i64 = dp.get(0).and_then(|s| s.parse().ok()).unwrap_or(0);
                let m: i64 = dp.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
                let d: i64 = dp.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
                tc_tz_suffix_ymd(tz_name, y, m, d)
            } else {
                norm
            };
            parse_tz_offset_s(&effective_tz).unwrap_or(0)
        };
        let new_tz_s = override_tz_str
            .as_deref()
            .and_then(parse_tz_offset_s)
            .unwrap_or(base_tz_s);
        // The effective TZ string: override > base > Z
        // When base_tz_raw contains a named timezone (e.g. "+01:00[Europe/Stockholm]"),
        // recalculate the DST offset for the FINAL date, not the source datetime's date.
        let tz = override_tz_str.clone().unwrap_or_else(|| {
            if base_tz_raw.is_empty() {
                "Z".to_owned()
            } else {
                let norm = normalize_tz(base_tz_raw);
                if let Some(brk) = norm.find('[') {
                    let tz_name = &norm[brk + 1..norm.len().saturating_sub(1)];
                    let dp: Vec<&str> = date_part.splitn(3, '-').collect();
                    let y: i64 = dp.get(0).and_then(|s| s.parse().ok()).unwrap_or(0);
                    let m: i64 = dp.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
                    let d: i64 = dp.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
                    tc_tz_suffix_ymd(tz_name, y, m, d)
                } else {
                    norm
                }
            }
        });
        // Apply UTC conversion if: base had TZ, override differs, override is provided.
        let (h, min, sec, ns) = if orig_had_tz && override_tz_str.is_some() && new_tz_s != base_tz_s
        {
            let base_wall_s = bh * 3600 + bmin * 60 + bsec;
            let utc_s = base_wall_s - base_tz_s;
            let new_wall_s = utc_s + new_tz_s;
            let new_h = ((new_wall_s / 3600) % 24 + 24) % 24;
            let new_min = (new_wall_s % 3600) / 60;
            let new_sec_v = new_wall_s % 60;
            (
                override_h.unwrap_or(new_h),
                override_min.unwrap_or(new_min.abs()),
                override_sec.unwrap_or(new_sec_v.abs()),
                if has_override_subsec {
                    let ms = temporal_get_i(pairs, "millisecond").unwrap_or(0);
                    let us = temporal_get_i(pairs, "microsecond").unwrap_or(0);
                    let ns_v = temporal_get_i(pairs, "nanosecond").unwrap_or(0);
                    ms * 1_000_000 + us * 1_000 + ns_v
                } else {
                    bns
                },
            )
        } else {
            (
                override_h.unwrap_or(bh),
                override_min.unwrap_or(bmin),
                override_sec.unwrap_or(bsec),
                if has_override_subsec {
                    let ms = temporal_get_i(pairs, "millisecond").unwrap_or(0);
                    let us = temporal_get_i(pairs, "microsecond").unwrap_or(0);
                    let ns_v = temporal_get_i(pairs, "nanosecond").unwrap_or(0);
                    ms * 1_000_000 + us * 1_000 + ns_v
                } else {
                    bns
                },
            )
        };
        let time_part = localtime_parts_to_str(h, min, sec, ns);
        return Some(format!("{date_part}T{time_part}{tz}"));
    }
    let tz = override_tz_str.unwrap_or_else(|| "Z".to_string());
    let h = temporal_get_i(pairs, "hour").unwrap_or(0);
    let min = temporal_get_i(pairs, "minute").unwrap_or(0);
    let sec = temporal_get_i(pairs, "second");
    let frac = temporal_sub_second(pairs);
    let time_part = match sec {
        None if frac.is_empty() => format!("{h:02}:{min:02}"),
        None => format!("{h:02}:{min:02}:00{frac}"),
        Some(s) => format!("{h:02}:{min:02}:{s:02}{frac}"),
    };
    Some(format!("{date_part}T{time_part}{tz}"))
}

/// Parsed components of an ISO 8601 duration like `P12Y5M14DT16H13M10.5S`.
struct ParsedDuration {
    years: i64,
    months: i64,
    days: i64,
    hours: i64,
    minutes: i64,
    seconds: i64,
    subsec_ns: i64,
}

// ── Duration arithmetic ───────────────────────────────────────────────────────

/// Check whether a Cypher-serialized value string is contained in a Cypher
/// list string (our `"[item1, item2, …]"` format).
///
/// Items are split by `", "` with bracket/quote-depth tracking so that nested
/// lists and single-quoted strings are handled correctly.
pub fn list_contains_str(list: &str, value: &str) -> bool {
    let inner = match list
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
    {
        Some(s) => s,
        None => return false,
    };
    if inner.is_empty() {
        return false;
    }
    let bytes = inner.as_bytes();
    let n = bytes.len();
    let mut depth: i32 = 0;
    let mut in_quote = false;
    let mut item_start = 0usize;
    let mut i = 0usize;
    loop {
        // Determine whether we're at a top-level separator or end-of-string.
        let at_sep = !in_quote
            && depth == 0
            && i + 1 < n
            && bytes[i] == b','
            && bytes[i + 1] == b' ';
        if at_sep || i == n {
            let item = &inner[item_start..i];
            if item == value {
                return true;
            }
            if at_sep {
                item_start = i + 2;
                i += 2;
                continue;
            } else {
                break; // end of string
            }
        }
        match bytes[i] {
            b'[' | b'{' if !in_quote => depth += 1,
            b']' | b'}' if !in_quote => depth -= 1,
            b'\'' if !in_quote => in_quote = true,
            b'\'' if in_quote => in_quote = false,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Add two ISO 8601 duration strings and return the result.
/// Returns `None` if either string is not a valid duration.
/// Carries ns→seconds→minutes→hours (but NOT hours→days or months→years).
pub fn duration_add_str(a: &str, b: &str) -> Option<String> {
    duration_arith_str(a, b, false)
}

/// Subtract duration `b` from `a` and return the ISO 8601 result.
pub fn duration_sub_str(a: &str, b: &str) -> Option<String> {
    duration_arith_str(a, b, true)
}

fn duration_arith_str(a: &str, b: &str, subtract: bool) -> Option<String> {
    let (a_neg, a_str) = if a.starts_with('-') { (true, &a[1..]) } else { (false, a) };
    let (b_neg, b_str) = if b.starts_with('-') { (true, &b[1..]) } else { (false, b) };
    let da = parse_duration_components(a_str)?;
    let db = parse_duration_components(b_str)?;

    // Sign-adjust components
    let sign_a: i64 = if a_neg { -1 } else { 1 };
    let sign_b: i64 = if b_neg { -1 } else { 1 };
    let sign_b = if subtract { -sign_b } else { sign_b };

    let y  = sign_a * da.years   + sign_b * db.years;
    let mo = sign_a * da.months  + sign_b * db.months;
    let d  = sign_a * da.days    + sign_b * db.days;

    // Time: accumulate in nanoseconds then carry ns→s→min→h
    let total_ns: i128 =
        (sign_a as i128) * (da.seconds as i128 * 1_000_000_000 + da.subsec_ns as i128)
      + (sign_b as i128) * (db.seconds as i128 * 1_000_000_000 + db.subsec_ns as i128);
    let (carry_min_from_s, s_ns) = ns_carry(total_ns);

    let total_min = sign_a * da.minutes + sign_b * db.minutes + carry_min_from_s;
    let (carry_h_from_min, min) = min_carry(total_min);

    let h = sign_a * da.hours + sign_b * db.hours + carry_h_from_min;

    Some(dur_fmt(y, mo, d, h, min, s_ns))
}

/// Multiply a duration by a floating-point number.
/// Cascades fractional components the same way `temporal_duration_from_map` does.
pub fn duration_mul_num_str(dur: &str, num: f64) -> Option<String> {
    if num == 1.0 {
        return Some(dur.to_owned());
    }
    let (neg, abs_str) = if dur.starts_with('-') { (true, &dur[1..]) } else { (false, dur) };
    let d = parse_duration_components(abs_str)?;
    let eff = if neg { -num } else { num };

    // Scale with cascading (same logic as temporal_duration_from_map).
    let years_f      = d.years   as f64 * eff;
    let months_f     = d.months  as f64 * eff + years_f.fract()  * 12.0;
    let days_f       = d.days    as f64 * eff + months_f.fract() * 30.436875;
    let hours_f      = d.hours   as f64 * eff + days_f.fract()   * 24.0;
    let minutes_f    = d.minutes as f64 * eff + hours_f.fract()  * 60.0;
    let secs_f       = d.seconds as f64 * eff + minutes_f.fract() * 60.0;
    let ns_extra_f   = d.subsec_ns as f64 * eff;

    let y  = years_f.trunc()  as i64;
    let mo = months_f.trunc() as i64;
    let dv = days_f.trunc()   as i64;

    // Seconds + ns → carry into minutes then hours.
    // Truncate sub-nanosecond fractions: 0.5 ns → 0 ns (Cypher semantics).
    let total_ns: i128 = (secs_f * 1_000_000_000.0).round() as i128
                       + ns_extra_f as i128;
    let (carry_min, s_ns) = ns_carry(total_ns);

    let total_min = minutes_f.trunc() as i64 + carry_min;
    let (carry_h, min) = min_carry(total_min);
    let h = hours_f.trunc() as i64 + carry_h;

    Some(dur_fmt(y, mo, dv, h, min, s_ns))
}

/// Divide a duration by a floating-point number.
pub fn duration_div_num_str(dur: &str, num: f64) -> Option<String> {
    if num == 0.0 {
        return None;
    }
    duration_mul_num_str(dur, 1.0 / num)
}

// ── Duration carry helpers ────────────────────────────────────────────────────

/// Carry nanoseconds into whole minutes + sub-minute ns.
/// Returns (carry_minutes: i64, remaining_ns_in_i128_for_dur_fmt).
fn ns_carry(total_ns: i128) -> (i64, i128) {
    let s_whole = if total_ns >= 0 {
        total_ns / 1_000_000_000
    } else {
        -((-total_ns) / 1_000_000_000)
    };
    let remain_ns = total_ns - s_whole * 1_000_000_000;
    let carry_min = if s_whole >= 0 { s_whole / 60 } else { -((-s_whole) / 60) };
    let s_final = s_whole - carry_min * 60;
    (carry_min as i64, s_final * 1_000_000_000 + remain_ns)
}

/// Carry whole minutes into whole hours + remaining minutes.
fn min_carry(total_min: i64) -> (i64, i64) {
    let carry_h = if total_min >= 0 { total_min / 60 } else { -((-total_min) / 60) };
    let min = total_min - carry_h * 60;
    (carry_h, min)
}

/// Parse an ISO 8601 duration string into its numeric components.
/// Returns None if the string is not a valid duration.
fn parse_duration_components(s: &str) -> Option<ParsedDuration> {
    if !s.starts_with('P') {
        return None;
    }

    let body = &s[1..];
    let t_pos = body.find('T');
    let date_str = t_pos.map_or(body, |p| &body[..p]);
    let time_str = t_pos.map_or("", |p| &body[p + 1..]);

    let parse_part = |part: &str, units: &[char]| -> Vec<f64> {
        let mut vals = vec![0.0f64; units.len()];
        let mut cur = String::new();
        for ch in part.chars() {
            if ch.is_ascii_digit() || ch == '.' {
                cur.push(ch);
            } else if ch == '-' && cur.is_empty() {
                cur.push('-');
            } else if !cur.is_empty() {
                if let Ok(v) = cur.parse::<f64>() {
                    let uc = ch.to_ascii_uppercase();
                    if let Some(idx) = units.iter().position(|&u| u == uc) {
                        vals[idx] = v;
                    }
                }
                cur.clear();
            }
        }
        vals
    };

    let dv = parse_part(date_str, &['Y', 'M', 'W', 'D']);
    let tv = parse_part(time_str, &['H', 'M', 'S']);

    let years = dv[0].trunc() as i64;
    let months = dv[1].trunc() as i64;
    let days = dv[3].trunc() as i64; // Weeks already cascaded to days in our construction
    let hours = tv[0].trunc() as i64;
    let minutes = tv[1].trunc() as i64;
    let secs_total_f = tv[2];
    // Use floor (not trunc) so that negative fractions work correctly:
    // floor(-59.9) = -60, giving subsec = 0.1s = 100_000_000 ns (non-negative).
    let sec_whole = secs_total_f.floor();
    let seconds = sec_whole as i64;
    let subsec_ns = ((secs_total_f - sec_whole) * 1_000_000_000.0).round() as i64;

    Some(ParsedDuration {
        years,
        months,
        days,
        hours,
        minutes,
        seconds,
        subsec_ns,
    })
}
pub(crate) fn duration_get_component(dur_str: &str, component: &str) -> Option<String> {
    let d = parse_duration_components(dur_str)?;
    let neg = dur_str.starts_with('-');
    let sign = if neg { -1 } else { 1 };
    let val: i64 = match component {
        "years" => sign * d.years,
        "quartersOfYear" | "quarters" => sign * (d.years * 4 + d.months / 3),
        "months" => sign * (d.years * 12 + d.months),
        "monthsOfQuarter" => sign * (d.months % 3),
        "monthsOfYear" => sign * (d.months % 12),
        "weeks" => sign * (d.days / 7),
        "daysOfWeek" => sign * (d.days % 7),
        "days" => sign * d.days,
        "hours" => sign * (d.days * 24 + d.hours),
        "minutesOfHour" => sign * d.minutes,
        "minutes" => sign * (d.days * 24 * 60 + d.hours * 60 + d.minutes),
        "secondsOfMinute" => sign * d.seconds,
        "seconds" => sign * (d.days * 24 * 3600 + d.hours * 3600 + d.minutes * 60 + d.seconds),
        "millisecondsOfSecond" => sign * (d.subsec_ns / 1_000_000),
        "milliseconds" => {
            let total_s = sign * (d.days * 24 * 3600 + d.hours * 3600 + d.minutes * 60 + d.seconds);
            total_s * 1_000 + sign * (d.subsec_ns / 1_000_000)
        }
        "microsecondsOfSecond" => sign * (d.subsec_ns / 1_000),
        "microseconds" => {
            let total_s = sign * (d.days * 24 * 3600 + d.hours * 3600 + d.minutes * 60 + d.seconds);
            total_s * 1_000_000 + sign * (d.subsec_ns / 1_000)
        }
        "nanosecondsOfSecond" => sign * d.subsec_ns,
        "nanoseconds" => {
            let total_s = sign * (d.days * 24 * 3600 + d.hours * 3600 + d.minutes * 60 + d.seconds);
            total_s * 1_000_000_000 + sign * d.subsec_ns
        }
        _ => return None,
    };
    Some(val.to_string())
}

/// Construct a `duration` literal (ISO 8601) from a map.
/// All fields are optional and can be integers or floats.
/// Fractional values cascade down: frac(weeks)*7→days, frac(days)*24→hours,
/// frac(hours)*60→minutes, frac(minutes)*60→seconds.
pub(crate) fn temporal_duration_from_map(pairs: &[(String, Expression)]) -> Option<String> {
    // Date components
    let years = temporal_get_f(pairs, "years").or_else(|| temporal_get_f(pairs, "year"));
    let months = temporal_get_f(pairs, "months").or_else(|| temporal_get_f(pairs, "month"));
    let weeks_raw = temporal_get_f(pairs, "weeks").or_else(|| temporal_get_f(pairs, "week"));
    let days_raw = temporal_get_f(pairs, "days").or_else(|| temporal_get_f(pairs, "day"));
    // Time components
    let hours_raw = temporal_get_f(pairs, "hours").or_else(|| temporal_get_f(pairs, "hour"));
    let minutes_raw = temporal_get_f(pairs, "minutes").or_else(|| temporal_get_f(pairs, "minute"));
    let seconds_raw = temporal_get_f(pairs, "seconds").or_else(|| temporal_get_f(pairs, "second"));
    let ms = temporal_get_f(pairs, "milliseconds").or_else(|| temporal_get_f(pairs, "millisecond"));
    let us = temporal_get_f(pairs, "microseconds").or_else(|| temporal_get_f(pairs, "microsecond"));
    let ns = temporal_get_f(pairs, "nanoseconds").or_else(|| temporal_get_f(pairs, "nanosecond"));

    if years.is_none()
        && months.is_none()
        && weeks_raw.is_none()
        && days_raw.is_none()
        && hours_raw.is_none()
        && minutes_raw.is_none()
        && seconds_raw.is_none()
        && ms.is_none()
        && us.is_none()
        && ns.is_none()
    {
        return None;
    }

    // Normalize fractional components by cascading down:
    // frac(months)*30.436875 → extra days (1 month = 365.2425/12 days)
    // weeks always convert to days (1 week = 7 days, no 'W' in output)
    // frac(days)*24 → extra hours, frac(hours)*60 → extra minutes,
    // frac(minutes)*60 → extra seconds.

    // Months: integer part stays as 'M'; fractional part → days
    let months_f = months.unwrap_or(0.0);
    let months_int = months_f.trunc();
    let extra_days_from_months = months_f.fract() * 30.436875;

    // Weeks: ALWAYS convert to days (never emit 'W')
    let weeks_f = weeks_raw.unwrap_or(0.0);
    let extra_days_from_weeks = weeks_f * 7.0;

    // Days = explicit days + cascade from weeks + cascade from months
    let days_total = days_raw.unwrap_or(0.0) + extra_days_from_weeks + extra_days_from_months;
    let days_int = days_total.trunc();
    let extra_hours_from_days = days_total.fract() * 24.0;

    let hours_total = hours_raw.unwrap_or(0.0) + extra_hours_from_days;
    let hours_int = hours_total.trunc();
    let extra_mins_from_hours = hours_total.fract() * 60.0;

    let mins_total = minutes_raw.unwrap_or(0.0) + extra_mins_from_hours;
    let mins_int = mins_total.trunc();
    let extra_secs_from_mins = mins_total.fract() * 60.0;

    let secs_total_f = seconds_raw.unwrap_or(0.0) + extra_secs_from_mins;

    // Build ISO 8601 duration: P[nY][nM][nD][T[nH][nM][nS]]
    let mut date_s = String::new();
    if let Some(y) = years {
        date_s.push_str(&format_duration_component(y, 'Y'));
    }
    if months_int != 0.0 {
        date_s.push_str(&format_duration_component(months_int, 'M'));
    }
    // Weeks are always converted to days — no 'W' emitted.
    if days_int != 0.0 {
        date_s.push_str(&format_duration_component(days_int, 'D'));
    }

    // Combine sub-second time parts into integer nanoseconds, then normalize
    // seconds → minutes using truncate-toward-zero carry.
    let ms_f = ms.unwrap_or(0.0);
    let us_f = us.unwrap_or(0.0);
    let ns_f = ns.unwrap_or(0.0);
    // Convert to nanoseconds (integer, rounding to nearest).
    let total_ns: i64 = (secs_total_f * 1_000_000_000.0).round() as i64
        + (ms_f * 1_000_000.0).round() as i64
        + (us_f * 1_000.0).round() as i64
        + ns_f.round() as i64;
    // Extract whole seconds (truncate toward zero) and sub-second remainder.
    let s_whole = if total_ns >= 0 {
        total_ns / 1_000_000_000
    } else {
        -((-total_ns) / 1_000_000_000)
    };
    let remain_ns = total_ns - s_whole * 1_000_000_000;
    // Carry whole seconds → minutes.
    let carry_min = if s_whole >= 0 {
        s_whole / 60
    } else {
        -((-s_whole) / 60)
    };
    let s_final = s_whole - carry_min * 60;
    // Combine carried minutes with cascaded integer minutes.
    let min_total = mins_int as i64 + carry_min;

    let mut time_s = String::new();
    if hours_int != 0.0 {
        time_s.push_str(&format_duration_component(hours_int, 'H'));
    }
    if min_total != 0 {
        time_s.push_str(&format!("{min_total}M"));
    }
    let sec_str = format_duration_secs(s_final, remain_ns);
    if !sec_str.is_empty() {
        time_s.push_str(&sec_str);
    }

    // has_time: explicit time fields OR cascade produced a non-empty time part.
    let has_time = hours_raw.is_some()
        || minutes_raw.is_some()
        || seconds_raw.is_some()
        || ms.is_some()
        || us.is_some()
        || ns.is_some()
        || !time_s.is_empty();

    let mut result = "P".to_string();
    result.push_str(&date_s);
    if has_time {
        result.push('T');
        result.push_str(&time_s);
    }
    if result == "P" || result == "PT" {
        result = "PT0S".to_string();
    }
    Some(result)
}

/// Format an integer-seconds + sub-second nanoseconds value as a duration seconds component.
/// Returns an empty string if both are zero.
fn format_duration_secs(s_whole: i64, remain_ns: i64) -> String {
    if s_whole == 0 && remain_ns == 0 {
        return String::new();
    }
    let neg = s_whole < 0 || (s_whole == 0 && remain_ns < 0);
    let abs_sw = s_whole.unsigned_abs();
    let abs_rn = remain_ns.unsigned_abs();
    if abs_rn == 0 {
        if neg {
            format!("-{abs_sw}S")
        } else {
            format!("{abs_sw}S")
        }
    } else {
        let frac = format!("{abs_rn:09}");
        let frac = frac.trim_end_matches('0');
        if neg {
            format!("-{abs_sw}.{frac}S")
        } else {
            format!("{abs_sw}.{frac}S")
        }
    }
}

fn format_duration_component(v: f64, suffix: char) -> String {
    if v == v.trunc() {
        format!("{}{}", v as i64, suffix)
    } else {
        // Remove trailing zeros from fractional representation
        let s = format!("{v}");
        format!("{s}{suffix}")
    }
}

fn format_duration_seconds(s: f64) -> String {
    if s == s.trunc() && s.fract() == 0.0 {
        format!("{}S", s as i64)
    } else {
        // Format with up to 9 decimal places, removing trailing zeros
        let formatted = format!("{:.9}", s);
        let trimmed = formatted.trim_end_matches('0');
        let trimmed = trimmed.trim_end_matches('.');
        format!("{trimmed}S")
    }
}

/// Parse an ISO 8601 date string to canonical "YYYY-MM-DD".
/// Also handles datetime strings by stripping the time part.
pub(crate) fn temporal_parse_date(s: &str) -> Option<String> {
    let s = s.trim();
    // If string contains 'T', strip the time part (accepts datetime strings).
    let s = if let Some(t_pos) = s.find('T') {
        &s[..t_pos]
    } else {
        s
    };
    // Extended-year format: ±YYYYYYY-MM-DD (year more/less than 4 digits)
    if s.starts_with('+') || s.starts_with('-') {
        let (sign, rest) = if s.starts_with('-') {
            (-1i64, &s[1..])
        } else {
            (1, &s[1..])
        };
        if let Some(ym_pos) = rest.find('-').filter(|&p| p >= 4) {
            let rest2 = &rest[ym_pos + 1..];
            if rest2.len() >= 5 && rest2.as_bytes().get(2) == Some(&b'-') {
                if let (Ok(y), Ok(m), Ok(d)) = (
                    rest[..ym_pos].parse::<i64>(),
                    rest2[..2].parse::<i64>(),
                    rest2[3..5].parse::<i64>(),
                ) {
                    let y = sign * y;
                    return Some(format!("{y}-{m:02}-{d:02}"));
                }
            }
        }
    }
    // Extended calendar: YYYY-MM-DD
    if s.len() == 10 && s.as_bytes().get(4) == Some(&b'-') && s.as_bytes().get(7) == Some(&b'-') {
        let y: i64 = s[..4].parse().ok()?;
        let m: i64 = s[5..7].parse().ok()?;
        let d: i64 = s[8..10].parse().ok()?;
        return Some(format!("{y:04}-{m:02}-{d:02}"));
    }
    // Basic calendar: YYYYMMDD
    if s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit()) {
        let y: i64 = s[..4].parse().ok()?;
        let m: i64 = s[4..6].parse().ok()?;
        let d: i64 = s[6..8].parse().ok()?;
        return Some(format!("{y:04}-{m:02}-{d:02}"));
    }
    // Extended year-month: YYYY-MM
    if s.len() == 7 && s.as_bytes().get(4) == Some(&b'-') {
        let y: i64 = s[..4].parse().ok()?;
        let m: i64 = s[5..7].parse().ok()?;
        return Some(format!("{y:04}-{m:02}-01"));
    }
    // Basic year-month: YYYYMM
    if s.len() == 6 && s.bytes().all(|b| b.is_ascii_digit()) {
        let y: i64 = s[..4].parse().ok()?;
        let m: i64 = s[4..6].parse().ok()?;
        return Some(format!("{y:04}-{m:02}-01"));
    }
    // Extended week: YYYY-Www-D or YYYY-Www
    if s.len() >= 8 && s.as_bytes().get(4) == Some(&b'-') && s.as_bytes().get(5) == Some(&b'W') {
        let y: i64 = s[..4].parse().ok()?;
        let w: i64 = s[6..8].parse().ok()?;
        let dow = if s.len() >= 10 && s.as_bytes().get(8) == Some(&b'-') {
            s[9..10].parse().ok()?
        } else {
            1i64
        };
        return Some(temporal_week_to_date(y, w, dow));
    }
    // Basic week: YYYYWwwD or YYYYWww
    if s.len() >= 7 && s.as_bytes().get(4) == Some(&b'W') {
        let y: i64 = s[..4].parse().ok()?;
        let w: i64 = s[5..7].parse().ok()?;
        let dow: i64 = if s.len() >= 8 {
            s[7..8].parse().ok()?
        } else {
            1i64
        };
        return Some(temporal_week_to_date(y, w, dow));
    }
    // Extended ordinal: YYYY-DDD
    if s.len() == 8 && s.as_bytes().get(4) == Some(&b'-') {
        let y: i64 = s[..4].parse().ok()?;
        let ord: i64 = s[5..8].parse().ok()?;
        let (m, d) = temporal_ordinal_to_md(y, ord);
        return Some(format!("{y:04}-{m:02}-{d:02}"));
    }
    // Basic ordinal: YYYYDDD
    if s.len() == 7 && s.bytes().all(|b| b.is_ascii_digit()) {
        let y: i64 = s[..4].parse().ok()?;
        let ord: i64 = s[4..7].parse().ok()?;
        let (m, d) = temporal_ordinal_to_md(y, ord);
        return Some(format!("{y:04}-{m:02}-{d:02}"));
    }
    // Year only: YYYY
    if s.len() == 4 && s.bytes().all(|b| b.is_ascii_digit()) {
        let y: i64 = s.parse().ok()?;
        return Some(format!("{y:04}-01-01"));
    }
    None
}

/// Parse an ISO 8601 local time string (no timezone).
pub(crate) fn temporal_parse_localtime(s: &str) -> Option<String> {
    let s = s.trim();
    // If string contains 'T', extract the time part only (from datetime/localdatetime strings).
    let s = if let Some(t_pos) = s.find('T') {
        &s[t_pos + 1..]
    } else {
        s
    };
    // Try stripping 'Z' or '+HH:MM' suffix for localtime (ignore timezone)
    let s = if s.ends_with('Z') {
        &s[..s.len() - 1]
    } else {
        s
    };
    // Remove timezone offset if present
    let s = if let Some(pos) = s.rfind(['+', '-']) {
        if pos >= 5 {
            &s[..pos]
        } else {
            s
        }
    } else {
        s
    };
    // HH:MM:SS.nnn or HH:MM:SS or HH:MM or HHMMSS.nnn etc.
    // Extended: HH:MM:SS.nnnnnnnnn (with optional fractional)
    if s.len() >= 5 && s.as_bytes().get(2) == Some(&b':') {
        let h: i64 = s[..2].parse().ok()?;
        let m: i64 = s[3..5].parse().ok()?;
        if s.len() == 5 {
            // HH:MM with no seconds — pad to HH:MM:00 for valid xsd:time format.
            return Some(format!("{h:02}:{m:02}:00"));
        }
        if s.as_bytes().get(5) == Some(&b':') {
            let sec_str = &s[6..];
            let (sec, frac) = if let Some(dot) = sec_str.find('.') {
                let sec_int: i64 = sec_str[..dot].parse().ok()?;
                let frac_str = &sec_str[dot..]; // includes the '.'
                (sec_int, frac_str.to_owned())
            } else {
                (sec_str.parse().ok()?, String::new())
            };
            return Some(format!("{h:02}:{m:02}:{sec:02}{frac}"));
        }
    }
    // Basic: HHMMSS or HHMM or HH
    if s.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        let h: i64 = s[..2].parse().ok()?;
        if s.len() == 2 {
            return Some(format!("{h:02}:00"));
        }
        let m: i64 = s[2..4].parse().ok()?;
        if s.len() == 4 {
            return Some(format!("{h:02}:{m:02}"));
        }
        if s.len() >= 6 {
            let sec_s = &s[4..6];
            let sec: i64 = sec_s.parse().ok()?;
            let frac = if s.len() > 6 { &s[6..] } else { "" };
            return Some(format!("{h:02}:{m:02}:{sec:02}{frac}"));
        }
    }
    None
}

/// Parse an ISO 8601 time string (with timezone).
pub(crate) fn temporal_parse_time(s: &str) -> Option<String> {
    let s = s.trim();
    // If string contains 'T', extract the time part only.
    let s = if let Some(t_pos) = s.find('T') {
        &s[t_pos + 1..]
    } else {
        s
    };
    let (time_body, tz_raw) = split_tz(s);
    let local = temporal_parse_localtime(time_body)?;
    // time() requires a timezone; default to Z (UTC) when none present.
    let tz = if tz_raw.is_empty() {
        "Z".to_owned()
    } else {
        normalize_tz(tz_raw)
    };
    // Always include seconds so that xsd:time literals are valid for SPARQL comparison.
    // "HH:MM" → "HH:MM:00"; "HH:MM:SS..." → unchanged.
    let local = if local.len() == 5 {
        format!("{local}:00")
    } else {
        local
    };
    Some(format!("{local}{tz}"))
}

/// Owned version of split_tz for use where borrowed lifetimes are inconvenient.
fn split_tz_owned(s: &str) -> (String, String) {
    let (b, t) = split_tz(s);
    (b.to_owned(), t.to_owned())
}

/// Strip a bracket-named timezone from a TZ string, keeping only the numeric offset.
/// "+01:00[Europe/Stockholm]" → "+01:00"
/// "[Europe/London]" → "Z"
/// "+05:00" → "+05:00"  (unchanged)
pub(crate) fn strip_named_tz(tz: &str) -> String {
    if let Some(brk) = tz.find('[') {
        let offset = &tz[..brk];
        if offset.is_empty() || offset == "Z" {
            "Z".to_owned()
        } else {
            offset.to_owned()
        }
    } else {
        tz.to_owned()
    }
}

/// Split a time or datetime string into (time_body, timezone_suffix).
/// Handles: Z, +HH:MM, -HH:MM, +HHMM, -HH, [Region/City], +HH:MM[Region/City].
fn split_tz(s: &str) -> (&str, &str) {
    if s.ends_with('Z') {
        return (&s[..s.len() - 1], "Z");
    }
    // Find the opening bracket for named timezone, if any.
    let bracket_pos = s.find('[');
    // Search for +/- that starts a numeric timezone offset,
    // looking backwards from before any bracket.
    let search_end = bracket_pos.unwrap_or(s.len());
    let bytes = s.as_bytes();
    for i in (2..search_end).rev() {
        if bytes[i] == b'+' || bytes[i] == b'-' {
            let after = &s[i + 1..search_end];
            // Must be followed by at least 2 digits
            if after.len() >= 2
                && after.as_bytes()[0].is_ascii_digit()
                && after.as_bytes()[1].is_ascii_digit()
            {
                // Include bracket region in tz if present: "+02:00[Europe/Stockholm]"
                return (&s[..i], &s[i..]);
            }
        }
    }
    // No numeric offset; only a bracket timezone (or nothing)
    if let Some(bracket) = bracket_pos {
        return (&s[..bracket], &s[bracket..]);
    }
    (s, "Z") // default UTC
}

/// Normalize timezone string.  Handles:
///   "Z" → "Z"
///   "+00:00" / "-00:00" / "+0000" / "+00" → "Z"
///   "+0100" → "+01:00"
///   "-04" → "-04:00"
///   "+HH:MM:SS" (historical) → "+HH:MM"
///   "+HH:MM" → "+HH:MM"
///   "+0845[Australia/Eucla]" → "+08:45[Australia/Eucla]"
///   "[Region/City]" → "[Region/City]"  (no offset lookup)
fn normalize_tz(tz: &str) -> String {
    if tz == "Z" || tz.is_empty() {
        return tz.to_owned();
    }
    if tz.starts_with('[') {
        // Named timezone only (no fixed offset available at compile time)
        return tz.to_owned();
    }
    let sign = &tz[..1];
    let rest = &tz[1..];
    // Split off any bracket region
    let (offset_str, region) = if let Some(b) = rest.find('[') {
        (&rest[..b], &rest[b..])
    } else {
        (rest, "")
    };
    // Normalize the numeric offset part
    let normalized = if offset_str == "00:00"
        || offset_str == "0000"
        || offset_str == "00"
        || offset_str.is_empty()
    {
        // UTC
        if region.is_empty() {
            return "Z".to_owned();
        }
        // UTC with named region: keep as +00:00[Region]
        format!("+00:00{region}")
    } else if offset_str.len() == 2 {
        // +HH → +HH:00
        format!("{sign}{offset_str}:00{region}")
    } else if offset_str.len() == 4 && !offset_str.contains(':') {
        // +HHMM → +HH:MM
        format!("{sign}{}:{}{region}", &offset_str[..2], &offset_str[2..])
    } else if offset_str.len() == 8
        && offset_str.as_bytes().get(2) == Some(&b':')
        && offset_str.as_bytes().get(5) == Some(&b':')
    {
        // +HH:MM:SS (historical) → +HH:MM
        format!("{sign}{}{region}", &offset_str[..5])
    } else {
        // +HH:MM or already normalized
        format!("{sign}{offset_str}{region}")
    };
    normalized
}

/// Parse an ISO 8601 localdatetime string (no timezone).
pub(crate) fn temporal_parse_localdatetime(s: &str) -> Option<String> {
    if let Some(t_pos) = s.find(['T', 't']) {
        let date_s = temporal_parse_date(&s[..t_pos])?;
        // Strip any timezone suffix for localdatetime
        let time_part = &s[t_pos + 1..];
        let (time_body, _tz) = split_tz(time_part);
        let time_s = temporal_parse_localtime(time_body)?;
        Some(format!("{date_s}T{time_s}"))
    } else {
        // Date-only string: time defaults to midnight (00:00).
        let date_s = temporal_parse_date(s)?;
        Some(format!("{date_s}T00:00"))
    }
}

/// Parse an ISO 8601 datetime string (with timezone).
pub(crate) fn temporal_parse_datetime(s: &str) -> Option<String> {
    let t_pos = s.find(['T', 't'])?;
    let date_s = temporal_parse_date(&s[..t_pos])?;
    let rest = &s[t_pos + 1..];
    let (time_body, tz_raw) = split_tz(rest);
    let time_s = temporal_parse_localtime(time_body)?;
    let tz = if tz_raw.starts_with('[') {
        // Named timezone without an explicit numeric offset: compute DST-aware offset from date.
        let tz_name = &tz_raw[1..tz_raw.len().saturating_sub(1)]; // strip '[' and ']'
        let dp: Vec<&str> = date_s.splitn(3, '-').collect();
        let y: i64 = dp.get(0).and_then(|s| s.parse().ok()).unwrap_or(0);
        let m: i64 = dp.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
        let d: i64 = dp.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
        tc_tz_suffix_ymd(tz_name, y, m, d)
    } else {
        normalize_tz(tz_raw)
    };
    Some(format!("{date_s}T{time_s}{tz}"))
}

/// Parse an ISO 8601 duration string.  We convert
/// "alternative" format P2012-02-02T... to the standard form, and
/// normalize fractional components (e.g. P0.75M → P22DT19H51M49.5S).
pub(crate) fn temporal_parse_duration(s: &str) -> Option<String> {
    if !s.starts_with('P') {
        return None;
    }
    let body = &s[1..];
    // Alternative format: PYYYY-MM-DDTHH:MM:SS.sss
    if body.contains('-')
        || (body.contains('T') && body.find('T').map(|p| &body[..p]).unwrap_or("").is_empty())
    {
        // Possibly "P2012-02-02T14:37:21.545" alternative format
        if let Some(result) = parse_duration_alternative(body) {
            return Some(result);
        }
    }
    // Standard format: P[nY][nM][nW][nD][T[nH][nM][nS]]
    // Normalize fractional components
    normalize_duration_iso(s)
}

/// Parse alternative ISO 8601 duration: PYYYY-MM-DDTHH:MM:SS.sss
fn parse_duration_alternative(body: &str) -> Option<String> {
    // Format: YYYY-MM-DDTHH:MM:SS.sss
    let t_pos = body.find(['T', 't'])?;
    let date_part = &body[..t_pos];
    let time_part = &body[t_pos + 1..];
    // Parse date: YYYY-MM-DD
    let date_parts: Vec<&str> = date_part.splitn(3, '-').collect();
    if date_parts.len() < 1 {
        return None;
    }
    let y: i64 = date_parts.get(0)?.parse().ok()?;
    let mo: i64 = date_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let d: i64 = date_parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    // Parse time: HH:MM:SS.sss
    let time_parts: Vec<&str> = time_part.splitn(3, ':').collect();
    let h: i64 = time_parts.get(0).and_then(|s| s.parse().ok()).unwrap_or(0);
    let min: i64 = time_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let sec_str = time_parts.get(2).copied().unwrap_or("0");
    let sec_s = format_duration_seconds(sec_str.parse::<f64>().ok()?);
    let mut result = String::from("P");
    if y != 0 {
        result.push_str(&format!("{y}Y"));
    }
    if mo != 0 {
        result.push_str(&format!("{mo}M"));
    }
    if d != 0 {
        result.push_str(&format!("{d}D"));
    }
    let has_time = h != 0 || min != 0 || sec_str != "0";
    if has_time {
        result.push('T');
        if h != 0 {
            result.push_str(&format!("{h}H"));
        }
        if min != 0 {
            result.push_str(&format!("{min}M"));
        }
        if sec_str != "0" && sec_str != "0.0" {
            result.push_str(&sec_s);
        }
    }
    if result == "P" {
        result.push_str("T0S");
    }
    Some(result)
}

/// Normalize fractional components in an ISO 8601 duration string.
/// e.g. "P5M1.5D" → "P5M1DT12H", "PT0.75M" → "PT45S"
/// Fractional weeks cascade to days, days to hours, hours to minutes, minutes to seconds.
fn normalize_duration_iso(s: &str) -> Option<String> {
    if !s.starts_with('P') {
        return None;
    }
    let body = &s[1..];

    // Split into date and time parts at 'T'.
    let t_pos = body.find('T');
    let date_str = t_pos.map_or(body, |p| &body[..p]);
    let time_str = t_pos.map_or("", |p| &body[p + 1..]);

    // Parse a run of ASCII digits / '.' characters followed by a unit letter.
    // Handles leading '-' for negative components (e.g., "-14D").
    let parse_components = |part: &str, units: &[char]| -> Vec<f64> {
        let mut vals = vec![0.0f64; units.len()];
        let mut cur = String::new();
        for ch in part.chars() {
            if ch.is_ascii_digit() || ch == '.' {
                cur.push(ch);
            } else if ch == '-' && cur.is_empty() {
                // Leading minus sign for a negative component value.
                cur.push('-');
            } else if !cur.is_empty() {
                if let Ok(v) = cur.parse::<f64>() {
                    let uc = ch.to_ascii_uppercase();
                    if let Some(idx) = units.iter().position(|&u| u == uc) {
                        vals[idx] = v;
                    }
                }
                cur.clear();
            }
        }
        vals
    };

    let dv = parse_components(date_str, &['Y', 'M', 'W', 'D']);
    let tv = parse_components(time_str, &['H', 'M', 'S']);

    let years_f = dv[0];
    let months_f = dv[1];
    let weeks_f = dv[2];
    let days_f = dv[3];
    let hours_f = tv[0];
    let mins_f = tv[1];
    let secs_f = tv[2];

    // Cascade fractional parts downward.
    // Months: integer part stays as 'M'; fractional part → days (1 month = 30.436875 days)
    let months_int = months_f.trunc();
    let extra_days_from_months = months_f.fract() * 30.436875;
    // Weeks: ALWAYS convert to days (never emit 'W')
    let extra_days_from_weeks = weeks_f * 7.0;

    let days_total = days_f + extra_days_from_weeks + extra_days_from_months;
    let days_int = days_total.trunc();
    let extra_hours = days_total.fract() * 24.0;

    let hours_total = hours_f + extra_hours;
    let hours_int = hours_total.trunc();
    let extra_mins = hours_total.fract() * 60.0;

    let mins_total = mins_f + extra_mins;
    let mins_int = mins_total.trunc();
    let extra_secs = mins_total.fract() * 60.0;

    let secs_total = secs_f + extra_secs;

    // Convert total seconds to ns for sub-second handling.
    let total_ns: i64 = (secs_total * 1_000_000_000.0).round() as i64;
    let s_whole = if total_ns >= 0 {
        total_ns / 1_000_000_000
    } else {
        -((-total_ns) / 1_000_000_000)
    };
    let remain_ns = total_ns - s_whole * 1_000_000_000;
    let carry_min = if s_whole >= 0 {
        s_whole / 60
    } else {
        -((-s_whole) / 60)
    };
    let s_final = s_whole - carry_min * 60;
    let min_total = mins_int as i64 + carry_min;

    // Build result string.
    let mut result = "P".to_string();
    if years_f != 0.0 {
        result.push_str(&format_duration_component(years_f, 'Y'));
    }
    if months_int != 0.0 {
        result.push_str(&format_duration_component(months_int, 'M'));
    }
    // Weeks are always converted to days — no 'W' emitted.
    if days_int != 0.0 {
        result.push_str(&format_duration_component(days_int, 'D'));
    }

    let mut time_s = String::new();
    if hours_int != 0.0 {
        time_s.push_str(&format_duration_component(hours_int, 'H'));
    }
    if min_total != 0 {
        time_s.push_str(&format!("{min_total}M"));
    }
    let sec_str = format_duration_secs(s_final, remain_ns);
    if !sec_str.is_empty() {
        time_s.push_str(&sec_str);
    }

    // Emit time section if input had a T separator or if cascade produced time.
    let has_time = t_pos.is_some() || !time_s.is_empty();
    if has_time {
        result.push('T');
        result.push_str(&time_s);
    }
    if result == "P" || result == "PT" {
        result = "PT0S".to_string();
    }
    Some(result)
}

/// Returns true if the expression is statically known to be a non-boolean value.
/// Used to detect compile-time type errors for boolean operators (AND/OR/XOR/NOT).
/// Null is excluded because `NOT null` → null in 3VL and is valid.
fn is_definitely_non_boolean(expr: &Expression) -> bool {
    matches!(
        expr,
        Expression::Literal(Literal::Integer(_))
            | Expression::Literal(Literal::Float(_))
            | Expression::Literal(Literal::String(_))
            | Expression::List(_)
            | Expression::Map(_)
    )
}

/// Returns true if the expression is statically known to be a non-list value,
/// which is invalid as the RHS of an IN expression.
fn is_definitely_non_list(expr: &Expression) -> bool {
    matches!(
        expr,
        Expression::Literal(Literal::Boolean(_))
            | Expression::Literal(Literal::Integer(_))
            | Expression::Literal(Literal::Float(_))
            | Expression::Literal(Literal::String(_))
            | Expression::Map(_)
    )
}

/// Try to evaluate an expression to a compile-time string literal.
/// Handles plain string literals and string concatenation (`'a' + 'b'`).
fn try_eval_to_str_literal(expr: &Expression) -> Option<String> {
    match expr {
        Expression::Literal(Literal::String(s)) => Some(s.clone()),
        Expression::Add(a, b) => {
            let sa = try_eval_to_str_literal(a)?;
            let sb = try_eval_to_str_literal(b)?;
            Some(format!("{sa}{sb}"))
        }
        _ => None,
    }
}

/// Extract an integer value from a literal integer expression (direct or negated).
fn get_literal_int(expr: &Expression) -> Option<i64> {
    match expr {
        Expression::Literal(Literal::Integer(n)) => Some(*n),
        Expression::Negate(inner) => match inner.as_ref() {
            Expression::Literal(Literal::Integer(n)) => Some(-n),
            _ => None,
        },
        _ => None,
    }
}

/// Three-valued logic conjunction: false beats null, null beats true.
fn tval_and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None, // null AND true = null, null AND null = null
    }
}

/// Evaluate a compile-time constant boolean expression (for use in list-append context).
/// Returns Some(Some(bool)) for definite true/false, Some(None) for null, None if not evaluable.
fn try_eval_bool_const(expr: &Expression) -> Option<Option<bool>> {
    match expr {
        Expression::Literal(Literal::Boolean(b)) => Some(Some(*b)),
        Expression::Literal(Literal::Null) => Some(None),
        // IS NULL: null IS NULL → true; any other literal → false
        Expression::IsNull(inner) => match inner.as_ref() {
            Expression::Literal(Literal::Null) => Some(Some(true)),
            Expression::Literal(_) => Some(Some(false)),
            _ => None,
        },
        // IS NOT NULL: null IS NOT NULL → false; any other literal → true
        Expression::IsNotNull(inner) => match inner.as_ref() {
            Expression::Literal(Literal::Null) => Some(Some(false)),
            Expression::Literal(_) => Some(Some(true)),
            _ => None,
        },
        Expression::Comparison(lhs, CompOp::In, rhs) => {
            if let Expression::List(items) = rhs.as_ref() {
                let mut found_null = false;
                for item in items {
                    match try_eval_literal_eq(lhs, item) {
                        Some(Some(true)) => return Some(Some(true)),
                        Some(None) => found_null = true,
                        _ => {}
                    }
                }
                Some(if found_null { None } else { Some(false) })
            } else {
                None
            }
        }
        Expression::Comparison(lhs, CompOp::Eq, rhs) => try_eval_literal_eq(lhs, rhs),
        Expression::Comparison(lhs, CompOp::Ne, rhs) => {
            try_eval_literal_eq(lhs, rhs).map(|r| r.map(|b| !b))
        }
        Expression::Comparison(lhs, op, rhs) => {
            // Null in any comparison → always null (3VL).
            let lhs_null = matches!(lhs.as_ref(), Expression::Literal(Literal::Null));
            let rhs_null = matches!(rhs.as_ref(), Expression::Literal(Literal::Null));
            if lhs_null || rhs_null {
                return Some(None);
            }
            // Evaluate numeric comparisons for literal integers/floats,
            // including arithmetic sub-expressions like Modulo, Add, etc.
            fn to_f64(e: &Expression) -> Option<f64> {
                match e {
                    Expression::Literal(Literal::Integer(n)) => Some(*n as f64),
                    Expression::Literal(Literal::Float(f)) => Some(*f),
                    // Handle negated literals like -15 → Negate(Integer(15))
                    Expression::Negate(inner) => to_f64(inner).map(|v| -v),
                    // Fold arithmetic at compile time
                    Expression::Modulo(a, b) => {
                        let av = to_f64(a)?;
                        let bv = to_f64(b)?;
                        if bv == 0.0 {
                            None
                        } else {
                            Some(av % bv)
                        }
                    }
                    Expression::Add(a, b) => Some(to_f64(a)? + to_f64(b)?),
                    Expression::Subtract(a, b) => Some(to_f64(a)? - to_f64(b)?),
                    Expression::Multiply(a, b) => Some(to_f64(a)? * to_f64(b)?),
                    _ => None,
                }
            }
            if let (Some(l), Some(r)) = (to_f64(lhs), to_f64(rhs)) {
                let result = match op {
                    CompOp::Lt => l < r,
                    CompOp::Le => l <= r,
                    CompOp::Gt => l > r,
                    CompOp::Ge => l >= r,
                    _ => return None,
                };
                Some(Some(result))
            } else {
                None
            }
        }
        Expression::Not(inner) => try_eval_bool_const(inner).map(|r| r.map(|b| !b)),
        Expression::And(a, b) => {
            let av = try_eval_bool_const(a)?;
            let bv = try_eval_bool_const(b)?;
            Some(tval_and(av, bv))
        }
        Expression::Or(a, b) => {
            let av = try_eval_bool_const(a)?;
            let bv = try_eval_bool_const(b)?;
            // Kleene 3VL OR: true if either true, false if both false, null otherwise
            match (av, bv) {
                (Some(true), _) | (_, Some(true)) => Some(Some(true)),
                (Some(false), Some(false)) => Some(Some(false)),
                _ => Some(None),
            }
        }
        _ => None,
    }
}

/// Evaluate equality of two literal expressions at compile time using Cypher's 3VL.
/// Returns Some(true/false/null) when both values are fully literal, None otherwise.
fn try_eval_literal_eq(lhs: &Expression, rhs: &Expression) -> Option<Option<bool>> {
    // Normalize arithmetic expressions to literal values where possible.
    fn normalize(e: &Expression) -> Option<Expression> {
        match e {
            Expression::Negate(inner) => match inner.as_ref() {
                Expression::Literal(Literal::Integer(n)) => {
                    Some(Expression::Literal(Literal::Integer(-*n)))
                }
                Expression::Literal(Literal::Float(f)) => {
                    Some(Expression::Literal(Literal::Float(-*f)))
                }
                _ => None,
            },
            // Fold modulo of integer literals at compile time
            Expression::Modulo(a, b) => {
                let an = normalize(a)?;
                let bn = normalize(b)?;
                match (&an, &bn) {
                    (
                        Expression::Literal(Literal::Integer(av)),
                        Expression::Literal(Literal::Integer(bv)),
                    ) => {
                        if *bv == 0 {
                            None
                        } else {
                            Some(Expression::Literal(Literal::Integer(*av % *bv)))
                        }
                    }
                    _ => None,
                }
            }
            // Fold addition of integer literals
            Expression::Add(a, b) => {
                let an = normalize(a)?;
                let bn = normalize(b)?;
                match (&an, &bn) {
                    (
                        Expression::Literal(Literal::Integer(av)),
                        Expression::Literal(Literal::Integer(bv)),
                    ) => Some(Expression::Literal(Literal::Integer(*av + *bv))),
                    _ => None,
                }
            }
            // Fold subtraction of integer literals
            Expression::Subtract(a, b) => {
                let an = normalize(a)?;
                let bn = normalize(b)?;
                match (&an, &bn) {
                    (
                        Expression::Literal(Literal::Integer(av)),
                        Expression::Literal(Literal::Integer(bv)),
                    ) => Some(Expression::Literal(Literal::Integer(*av - *bv))),
                    _ => None,
                }
            }
            _ => Some(e.clone()),
        }
    }
    let lhs_n = normalize(lhs)?;
    let rhs_n = normalize(rhs)?;
    let lhs = &lhs_n;
    let rhs = &rhs_n;
    match (lhs, rhs) {
        // Any null input → null output
        (Expression::Literal(Literal::Null), _) | (_, Expression::Literal(Literal::Null)) => {
            Some(None)
        }
        // Scalar comparisons (same type)
        (Expression::Literal(Literal::Integer(a)), Expression::Literal(Literal::Integer(b))) => {
            Some(Some(a == b))
        }
        (Expression::Literal(Literal::Float(a)), Expression::Literal(Literal::Float(b))) => {
            Some(Some(a == b))
        }
        (Expression::Literal(Literal::String(a)), Expression::Literal(Literal::String(b))) => {
            Some(Some(a == b))
        }
        (Expression::Literal(Literal::Boolean(a)), Expression::Literal(Literal::Boolean(b))) => {
            Some(Some(a == b))
        }
        // Numeric cross-type: Integer == Float (Cypher promotes numerics for equality)
        (Expression::Literal(Literal::Integer(a)), Expression::Literal(Literal::Float(b))) => {
            if b.is_nan() {
                Some(None) // NaN comparisons → null
            } else {
                Some(Some((*a as f64) == *b))
            }
        }
        (Expression::Literal(Literal::Float(a)), Expression::Literal(Literal::Integer(b))) => {
            if a.is_nan() {
                Some(None) // NaN comparisons → null
            } else {
                Some(Some(*a == (*b as f64)))
            }
        }
        // Different scalar types (no nulls) → false
        (Expression::Literal(_), Expression::Literal(_)) => Some(Some(false)),
        // Both lists
        (Expression::List(a), Expression::List(b)) => {
            if a.len() != b.len() {
                return Some(Some(false));
            }
            let mut result: Option<bool> = Some(true);
            for (ax, bx) in a.iter().zip(b.iter()) {
                let pair = try_eval_literal_eq(ax, bx)?; // None = can't evaluate
                result = tval_and(result, pair);
                if result == Some(false) {
                    break;
                }
            }
            Some(result)
        }
        // List vs scalar (non-null) → false
        (Expression::List(_), Expression::Literal(_))
        | (Expression::Literal(_), Expression::List(_)) => Some(Some(false)),
        // Both maps
        (Expression::Map(a), Expression::Map(b)) => {
            // Build key sets
            let a_keys: Vec<&str> = a.iter().map(|(k, _)| k.as_str()).collect();
            let b_keys: Vec<&str> = b.iter().map(|(k, _)| k.as_str()).collect();
            // Key sets must match exactly (order-insensitive)
            let mut a_sorted = a_keys.clone();
            a_sorted.sort_unstable();
            let mut b_sorted = b_keys.clone();
            b_sorted.sort_unstable();
            if a_sorted != b_sorted {
                return Some(Some(false));
            }
            // Compare values for each key using 3VL
            let mut result: Option<bool> = Some(true);
            for (key, a_val) in a.iter() {
                if let Some((_, b_val)) = b.iter().find(|(k, _)| k == key) {
                    let pair = try_eval_literal_eq(a_val, b_val)?;
                    result = tval_and(result, pair);
                    if result == Some(false) {
                        break;
                    }
                }
            }
            Some(result)
        }
        // Map vs scalar
        (Expression::Map(_), Expression::Literal(_))
        | (Expression::Literal(_), Expression::Map(_)) => Some(Some(false)),
        // Can't evaluate at compile time
        _ => None,
    }
}

/// Serialize a single expression as a list element (no outer brackets).
fn serialize_list_element(e: &Expression) -> String {
    match e {
        Expression::Literal(Literal::Integer(n)) => n.to_string(),
        Expression::Literal(Literal::Float(f)) => cypher_float_str(*f),
        Expression::Literal(Literal::String(s)) => format!("'{s}'"),
        Expression::Literal(Literal::Boolean(b)) => b.to_string(),
        Expression::Literal(Literal::Null) => "null".to_string(),
        Expression::List(inner) => serialize_list_literal(inner),
        Expression::Map(pairs) => {
            // Serialize map as "{key: value, ...}" string
            let entries: Vec<String> = pairs
                .iter()
                .map(|(k, v)| format!("{k}: {}", serialize_list_element(v)))
                .collect();
            format!("{{{}}}", entries.join(", "))
        }
        Expression::Negate(inner) => match inner.as_ref() {
            Expression::Literal(Literal::Integer(n)) => format!("-{n}"),
            Expression::Literal(Literal::Float(f)) => cypher_float_str(-f),
            _ => "?".to_string(),
        },
        _ => "?".to_string(),
    }
}

/// Evaluate a list comprehension projection for a single literal element.
/// Returns the serialized string form of the result, or None if unevaluable.
fn eval_comprehension_item(var: &str, val: &Expression, proj: &Expression) -> Option<String> {
    match proj {
        // Projection is just the variable → pass element through
        Expression::Variable(v) if v == var => Some(serialize_list_element(val)),
        // Projection is a function call
        Expression::FunctionCall { name, args, .. } => {
            if args.len() != 1 {
                return None;
            }
            // Resolve the single argument (must be the variable or a literal)
            let resolved = match &args[0] {
                Expression::Variable(v) if v == var => val.clone(),
                Expression::Literal(_) => args[0].clone(),
                _ => return None,
            };
            let lit = match &resolved {
                Expression::Literal(l) => l,
                _ => return None,
            };
            match name.to_ascii_lowercase().as_str() {
                "tointeger" => {
                    let result = match lit {
                        Literal::Integer(n) => Literal::Integer(*n),
                        Literal::Float(f) => Literal::Integer(*f as i64),
                        Literal::String(s) => {
                            if let Ok(n) = s.parse::<i64>() {
                                Literal::Integer(n)
                            } else if let Ok(f) = s.parse::<f64>() {
                                Literal::Integer(f as i64)
                            } else {
                                Literal::Null
                            }
                        }
                        Literal::Null => Literal::Null,
                        // Boolean → TypeError in Cypher; return None so the
                        // runtime SPARQL path raises the error instead.
                        _ => return None,
                    };
                    Some(serialize_list_element(&Expression::Literal(result)))
                }
                "tofloat" => {
                    let result = match lit {
                        Literal::Integer(n) => Literal::Float(*n as f64),
                        Literal::Float(f) => Literal::Float(*f),
                        Literal::String(s) => {
                            if let Ok(f) = s.parse::<f64>() {
                                Literal::Float(f)
                            } else {
                                Literal::Null
                            }
                        }
                        Literal::Null => Literal::Null,
                        // Boolean → TypeError in Cypher; return None so the
                        // runtime SPARQL path raises the error instead.
                        _ => return None,
                    };
                    Some(serialize_list_element(&Expression::Literal(result)))
                }
                "tostring" => {
                    let s = match lit {
                        Literal::Integer(n) => n.to_string(),
                        Literal::Float(f) => cypher_float_str(*f),
                        Literal::Boolean(b) => b.to_string(),
                        Literal::String(s) => s.clone(),
                        Literal::Null => return Some("null".to_string()),
                    };
                    Some(format!("'{s}'"))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Join two `GraphPattern`s, merging adjacent BGPs where possible.
fn join_patterns(left: GraphPattern, right: GraphPattern) -> GraphPattern {
    // Identity: join with empty BGP is a no-op.
    if let GraphPattern::Bgp { patterns } = &left {
        if patterns.is_empty() {
            return right;
        }
    }
    if let GraphPattern::Bgp { patterns } = &right {
        if patterns.is_empty() {
            return left;
        }
    }
    // Merge two BGPs into one (flattening is valid in SPARQL).
    match (left, right) {
        (GraphPattern::Bgp { patterns: mut lp }, GraphPattern::Bgp { patterns: rp }) => {
            lp.extend(rp);
            GraphPattern::Bgp { patterns: lp }
        }
        (l, r) => GraphPattern::Join {
            left: Box::new(l),
            right: Box::new(r),
        },
    }
}

/// Apply an iterator of filter expressions to a pattern, innermost first.
fn apply_filters(
    mut pattern: GraphPattern,
    filters: impl Iterator<Item = SparExpr>,
) -> GraphPattern {
    for expr in filters {
        pattern = GraphPattern::Filter {
            expr,
            inner: Box::new(pattern),
        };
    }
    pattern
}

/// Convert a `TermPattern` to a `SparExpr` for use in FILTER comparisons.
fn term_to_sparexpr(tp: &TermPattern) -> SparExpr {
    match tp {
        TermPattern::Variable(v) => SparExpr::Variable(v.clone()),
        TermPattern::NamedNode(n) => SparExpr::Literal(SparLit::new_simple_literal(n.as_str())),
        TermPattern::Literal(lit) => SparExpr::Literal(lit.clone()),
        TermPattern::BlankNode(b) => SparExpr::Literal(SparLit::new_simple_literal(b.as_str())),
        TermPattern::Triple(_) => SparExpr::Literal(SparLit::new_simple_literal("__triple__")),
    }
}

/// Build a CONCAT(STR(s), "|", STR(p), "|", STR(o)) expression that serves as
/// a canonical edge identity.  The caller must pass the terms in the actual
/// stored-triple order (subject, predicate, object) so that forward and reverse
/// UNION branches of an undirected match produce identical IDs.
fn build_edge_id_expr(s: &TermPattern, p_expr: SparExpr, o: &TermPattern) -> SparExpr {
    use spargebra::algebra::Function;
    let sep = SparExpr::Literal(SparLit::new_simple_literal("|"));
    SparExpr::FunctionCall(
        Function::Concat,
        vec![
            SparExpr::FunctionCall(Function::Str, vec![term_to_sparexpr(s)]),
            sep.clone(),
            SparExpr::FunctionCall(Function::Str, vec![p_expr]),
            sep,
            SparExpr::FunctionCall(Function::Str, vec![term_to_sparexpr(o)]),
        ],
    )
}

/// Convert a `NamedNodePattern` to a `SparExpr` for use in FILTER comparisons.
fn named_node_to_sparexpr(nnp: &spargebra::term::NamedNodePattern) -> SparExpr {
    use spargebra::term::NamedNodePattern;
    match nnp {
        NamedNodePattern::Variable(v) => SparExpr::Variable(v.clone()),
        NamedNodePattern::NamedNode(n) => {
            SparExpr::Literal(SparLit::new_simple_literal(n.as_str()))
        }
    }
}

/// Convert a `TermPattern` (literal or named node) to a `GroundTerm` for use
/// in `GraphPattern::Values` bindings. Returns an error for variable terms.
fn term_pattern_to_ground(tp: TermPattern) -> Result<GroundTerm, PolygraphError> {
    match tp {
        TermPattern::NamedNode(nn) => Ok(GroundTerm::NamedNode(nn)),
        TermPattern::Literal(lit) => Ok(GroundTerm::Literal(lit)),
        TermPattern::BlankNode(_) => Err(PolygraphError::UnsupportedFeature {
            feature: "blank node in UNWIND list literal — only ground terms supported".to_string(),
        }),
        TermPattern::Variable(_) => Err(PolygraphError::UnsupportedFeature {
            feature: "variable in UNWIND list literal — only literal values supported".to_string(),
        }),
        TermPattern::Triple(_) => Err(PolygraphError::UnsupportedFeature {
            feature: "triple pattern in UNWIND list — not a ground term".to_string(),
        }),
    }
}

// ── Temporal duration between two temporal values ─────────────────────────────

/// A parsed temporal value used for duration computation.
struct TempoVal {
    has_date: bool,
    has_time: bool,
    /// Whether an explicit timezone offset was present (Z or ±HH:MM).
    has_tz: bool,
    year: i64,
    month: i64,
    day: i64,
    /// Raw time from string (no TZ adjustment), nanoseconds from midnight.
    local_time_ns: i128,
    /// TZ-adjusted to UTC, nanoseconds from midnight.
    utc_time_ns: i128,
}

/// Parse a time string "HH:MM[:SS[.nnn]][±HH:MM|Z]" into
/// (local_time_ns, tz_offset_seconds, has_tz).
fn parse_time_ns_ext(s: &str) -> Option<(i128, i64, bool)> {
    let s = s.trim();
    // Strip named timezone [...]
    let s = if let Some(b) = s.rfind('[') {
        &s[..b]
    } else {
        s
    };
    // Detect trailing TZ offset: Z or ±HH[:MM], starting at pos ≥ 5
    let tz_start = s
        .rfind(|c: char| c == 'Z' || c == '+' || c == '-')
        .filter(|&p| p >= 5)
        .unwrap_or(s.len());
    let tz_raw = &s[tz_start..];
    let body = &s[..tz_start];
    let (tz_secs, has_tz) = if tz_raw.is_empty() {
        (0i64, false)
    } else if tz_raw.eq_ignore_ascii_case("z") {
        (0, true)
    } else {
        let neg = tz_raw.starts_with('-');
        let tz = &tz_raw[1..];
        let (th, tm) = if tz.contains(':') {
            let p = tz.find(':').unwrap();
            let th: i64 = tz[..p].parse().ok()?;
            let tm: i64 = tz[p + 1..].parse().ok()?;
            (th, tm)
        } else if tz.len() == 4 {
            let th: i64 = tz[..2].parse().ok()?;
            let tm: i64 = tz[2..4].parse().ok()?;
            (th, tm)
        } else if tz.len() >= 2 {
            let th: i64 = tz.parse().ok()?;
            (th, 0)
        } else {
            (0, 0)
        };
        let secs = th * 3600 + tm * 60;
        (if neg { -secs } else { secs }, true)
    };
    if body.len() < 5 || body.as_bytes().get(2) != Some(&b':') {
        return None;
    }
    let h: i64 = body[..2].parse().ok()?;
    let m: i64 = body[3..5].parse().ok()?;
    let sec_ns: i128 = if body.len() > 5 && body.as_bytes().get(5) == Some(&b':') {
        let sec_str = &body[6..];
        if let Some(dot) = sec_str.find('.') {
            let whole: i64 = sec_str[..dot].parse().ok()?;
            let frac_str = &sec_str[dot + 1..];
            let mut ns_str = frac_str.to_string();
            ns_str.truncate(9);
            while ns_str.len() < 9 {
                ns_str.push('0');
            }
            let ns: i64 = ns_str.parse().ok()?;
            (whole as i128) * 1_000_000_000 + ns as i128
        } else {
            let whole: i64 = sec_str.parse().ok()?;
            (whole as i128) * 1_000_000_000
        }
    } else {
        0
    };
    let local_ns = (h as i128) * 3_600_000_000_000 + (m as i128) * 60_000_000_000 + sec_ns;
    Some((local_ns, tz_secs, has_tz))
}

/// Parse a date string "YYYY-MM-DD" (or with leading ±) into (year, month, day).
fn parse_date_ymd(s: &str) -> Option<(i64, i64, i64)> {
    let s = s.trim();
    // Handle leading + or - for year sign
    let (sign, rest) = if s.starts_with('+') {
        (1i64, &s[1..])
    } else if s.starts_with('-') {
        (-1, &s[1..])
    } else {
        (1, s)
    };
    // Find year-month separator '-' after at least 4 year digits.
    // This handles both standard "1984-10-11" (ym_pos=4) and
    // extended-year "999999999-01-01" (ym_pos=9).
    let ym_pos = rest.find('-').filter(|&p| p >= 4)?;
    let y: i64 = rest[..ym_pos].parse().ok()?;
    let rest2 = &rest[ym_pos + 1..];
    if rest2.len() < 5 || rest2.as_bytes().get(2) != Some(&b'-') {
        return None;
    }
    let m: i64 = rest2[..2].parse().ok()?;
    let d: i64 = rest2[3..5].parse().ok()?;
    Some((sign * y, m, d))
}

/// Parse any Cypher temporal string into a `TempoVal`.
fn temporal_to_val(s: &str) -> Option<TempoVal> {
    let s = s.trim();
    // Strip named timezone suffix [...]
    let s = if let Some(b) = s.rfind('[') {
        &s[..b]
    } else {
        s
    };
    let s = s.trim_end_matches(']');

    if let Some(t_pos) = s.find('T') {
        // datetime or localdatetime
        let date_part = &s[..t_pos];
        let time_raw = &s[t_pos + 1..];
        let (y, m, d) = parse_date_ymd(date_part)?;
        let (local_ns, tz_secs, has_tz) = parse_time_ns_ext(time_raw)?;
        let utc_ns = local_ns - (tz_secs as i128) * 1_000_000_000;
        Some(TempoVal {
            has_date: true,
            has_time: true,
            has_tz,
            year: y,
            month: m,
            day: d,
            local_time_ns: local_ns,
            utc_time_ns: utc_ns,
        })
    } else if s.len() >= 3 && s.as_bytes().get(2) == Some(&b':') {
        // time or localtime: HH:MM...
        let (local_ns, tz_secs, has_tz) = parse_time_ns_ext(s)?;
        let utc_ns = local_ns - (tz_secs as i128) * 1_000_000_000;
        Some(TempoVal {
            has_date: false,
            has_time: true,
            has_tz,
            year: 0,
            month: 0,
            day: 0,
            local_time_ns: local_ns,
            utc_time_ns: utc_ns,
        })
    } else {
        // date: YYYY-MM-DD
        let (y, m, d) = parse_date_ymd(s)?;
        Some(TempoVal {
            has_date: true,
            has_time: false,
            has_tz: false,
            year: y,
            month: m,
            day: d,
            local_time_ns: 0,
            utc_time_ns: 0,
        })
    }
}

/// Choose the appropriate time_ns for comparison: UTC when both have_tz, local otherwise.
fn tempo_time(v: &TempoVal, use_utc: bool) -> i128 {
    if use_utc {
        v.utc_time_ns
    } else {
        v.local_time_ns
    }
}

const DAY_NS: i128 = 86_400_000_000_000;

/// Format signed seconds-in-nanoseconds as "NNS" or "NN.fS".
fn dur_fmt_sec_ns(ns: i128) -> String {
    if ns == 0 {
        return String::new();
    }
    let neg = ns < 0;
    let abs_ns = ns.unsigned_abs();
    let whole = (abs_ns / 1_000_000_000) as i64;
    let frac_ns = (abs_ns % 1_000_000_000) as i64;
    let frac_part = if frac_ns == 0 {
        String::new()
    } else {
        let s = format!("{frac_ns:09}");
        format!(".{}", s.trim_end_matches('0'))
    };
    if neg {
        format!("-{whole}{frac_part}S")
    } else {
        format!("{whole}{frac_part}S")
    }
}

/// Format a duration as ISO 8601 string.  All non-zero components must have the same sign.
fn dur_fmt(y: i64, mo: i64, d: i64, h: i64, min: i64, s_ns: i128) -> String {
    if y == 0 && mo == 0 && d == 0 && h == 0 && min == 0 && s_ns == 0 {
        return "PT0S".to_string();
    }
    let mut result = String::from("P");
    if y != 0 {
        result.push_str(&format!("{y}Y"));
    }
    if mo != 0 {
        result.push_str(&format!("{mo}M"));
    }
    if d != 0 {
        result.push_str(&format!("{d}D"));
    }
    if h != 0 || min != 0 || s_ns != 0 {
        result.push('T');
        if h != 0 {
            result.push_str(&format!("{h}H"));
        }
        if min != 0 {
            result.push_str(&format!("{min}M"));
        }
        if s_ns != 0 {
            result.push_str(&dur_fmt_sec_ns(s_ns));
        }
    }
    result
}

/// Split total nanoseconds into (hours, minutes, seconds_ns) with uniform sign.
fn split_ns_to_hms(total_ns: i128) -> (i64, i64, i128) {
    if total_ns == 0 {
        return (0, 0, 0);
    }
    let neg = total_ns < 0;
    let abs = total_ns.unsigned_abs();
    let h = (abs / 3_600_000_000_000) as i64;
    let rem = abs % 3_600_000_000_000;
    let m = (rem / 60_000_000_000) as i64;
    let s = (rem % 60_000_000_000) as i128;
    if neg {
        (-(h as i64), -(m as i64), -(s))
    } else {
        (h as i64, m as i64, s)
    }
}

/// Calendar diff (positive direction only): returns (yd, md, dd) where all ≥ 0.
fn calendar_diff_pos(y1: i64, m1: i64, d1: i64, y2: i64, m2: i64, d2: i64) -> (i64, i64, i64) {
    let mut yd = y2 - y1;
    let mut md = m2 - m1;
    let mut dd = d2 - d1;
    if dd < 0 {
        md -= 1;
        let pm = if m2 == 1 { 12 } else { m2 - 1 };
        let py = if m2 == 1 { y2 - 1 } else { y2 };
        dd += temporal_dim(py, pm);
    }
    if md < 0 {
        yd -= 1;
        md += 12;
    }
    (yd, md, dd)
}

/// Compute `duration.between(lhs, rhs)`.
pub(crate) fn temporal_duration_between(lhs: &str, rhs: &str) -> Option<String> {
    let l = temporal_to_val(lhs)?;
    let r = temporal_to_val(rhs)?;
    let use_utc = l.has_tz && r.has_tz;
    let l_t = if l.has_time {
        tempo_time(&l, use_utc)
    } else {
        0
    };
    let r_t = if r.has_time {
        tempo_time(&r, use_utc)
    } else {
        0
    };

    if !l.has_date || !r.has_date {
        // Pure time result.
        let diff = r_t - l_t;
        let (h, min, s) = split_ns_to_hms(diff);
        return Some(dur_fmt(0, 0, 0, h, min, s));
    }

    // Both have date component.
    let epoch_l = temporal_epoch(l.year, l.month, l.day);
    let epoch_r = temporal_epoch(r.year, r.month, r.day);
    let epoch_diff = epoch_r - epoch_l;

    if epoch_diff >= 0 {
        // Positive direction: use calendar form Y/M/D.
        let (mut yd, mut md, mut dd) =
            calendar_diff_pos(l.year, l.month, l.day, r.year, r.month, r.day);
        let mut t_diff = r_t - l_t;
        if t_diff < 0 {
            // Borrow 1 day.
            if dd > 0 {
                dd -= 1;
            } else if md > 0 {
                md -= 1;
                let pm = if r.month == 1 { 12 } else { r.month - 1 };
                let py = if r.month == 1 { r.year - 1 } else { r.year };
                dd += temporal_dim(py, pm) - 1;
            } else if yd > 0 {
                yd -= 1;
                md = 11;
                let pm = if r.month == 1 { 12 } else { r.month - 1 };
                let py = if r.month == 1 { r.year - 1 } else { r.year };
                dd += temporal_dim(py, pm) - 1;
            }
            t_diff += DAY_NS;
        }
        let (h, min, s) = split_ns_to_hms(t_diff);
        Some(dur_fmt(yd, md, dd, h, min, s))
    } else {
        // Negative direction: use epoch days (no Y/M).
        let mut days = epoch_diff;
        let mut t_diff = r_t - l_t;
        if t_diff > 0 {
            // Borrow 1 backward day.
            days += 1;
            t_diff -= DAY_NS;
        }
        let (h, min, s) = split_ns_to_hms(t_diff);
        Some(dur_fmt(0, 0, days, h, min, s))
    }
}

/// Compute `duration.inMonths(lhs, rhs)`.
pub(crate) fn temporal_duration_in_months(lhs: &str, rhs: &str) -> Option<String> {
    let l = temporal_to_val(lhs)?;
    let r = temporal_to_val(rhs)?;
    if !l.has_date || !r.has_date {
        return Some("PT0S".to_string());
    }
    let use_utc = l.has_tz && r.has_tz;
    let l_t = if l.has_time {
        tempo_time(&l, use_utc)
    } else {
        0
    };
    let r_t = if r.has_time {
        tempo_time(&r, use_utc)
    } else {
        0
    };

    let mut raw_months = (r.year - l.year) * 12 + (r.month - l.month);
    // Day/time comparison for sub-month adjustment.
    let r_day_ns = (r.day as i128) * DAY_NS + r_t;
    let l_day_ns = (l.day as i128) * DAY_NS + l_t;
    if raw_months >= 0 {
        if r_day_ns < l_day_ns {
            raw_months -= 1;
        }
    } else {
        if r_day_ns > l_day_ns {
            raw_months += 1;
        }
    }
    if raw_months == 0 {
        return Some("PT0S".to_string());
    }
    let y = raw_months / 12;
    let m = raw_months % 12;
    Some(dur_fmt(y, m, 0, 0, 0, 0))
}

/// Compute `duration.inDays(lhs, rhs)`.
/// Returns the truncated whole-day difference, accounting for time-of-day.
pub(crate) fn temporal_duration_in_days(lhs: &str, rhs: &str) -> Option<String> {
    let l = temporal_to_val(lhs)?;
    let r = temporal_to_val(rhs)?;
    if !l.has_date || !r.has_date {
        return Some("PT0S".to_string());
    }
    let use_utc = l.has_tz && r.has_tz;
    let l_t = if l.has_time {
        tempo_time(&l, use_utc)
    } else {
        0
    };
    let r_t = if r.has_time {
        tempo_time(&r, use_utc)
    } else {
        0
    };
    let l_epoch_ns = temporal_epoch(l.year, l.month, l.day) as i128 * DAY_NS;
    let r_epoch_ns = temporal_epoch(r.year, r.month, r.day) as i128 * DAY_NS;
    // Truncate total ns toward zero to get whole days.
    let total_diff_ns = (r_epoch_ns + r_t) - (l_epoch_ns + l_t);
    let days = (total_diff_ns / DAY_NS) as i64; // truncates toward zero
    if days == 0 {
        return Some("PT0S".to_string());
    }
    Some(format!("P{days}D"))
}

/// Compute `duration.inSeconds(lhs, rhs)`.
pub(crate) fn temporal_duration_in_seconds(lhs: &str, rhs: &str) -> Option<String> {
    let l = temporal_to_val(lhs)?;
    let r = temporal_to_val(rhs)?;

    // When one operand has a named timezone and the other is timezone-unaware
    // (localdatetime, localtime, or date), the TZ-unaware operand is treated
    // as being in the same named timezone.  The UTC offset for the TZ-unaware
    // operand is determined by the DST rule at its own date/time, which on a
    // DST transition day depends on the hour (e.g. Oct 29 00:00 = summer,
    // 04:00 = winter).  Both are then compared as UTC instants.
    let (l_t, r_t) = if l.has_tz && r.has_tz {
        // Both have explicit TZ: compare as UTC instants.
        (l.utc_time_ns, r.utc_time_ns)
    } else if l.has_tz && !r.has_tz {
        // Only lhs has TZ.  Apply lhs's named timezone to rhs.
        let named_tz = extract_named_tz(lhs);
        if !named_tz.is_empty() {
            // For rhs with no date, use lhs's date as context.
            let (ry, rm, rd) = if r.has_date {
                (r.year, r.month, r.day)
            } else {
                (l.year, l.month, l.day)
            };
            let r_hour = if r.has_time {
                ((r.local_time_ns / 3_600_000_000_000).abs()) as i64
            } else {
                0i64 // midnight
            };
            let r_off_s = parse_tz_offset_s(&tc_tz_suffix_ymdh(named_tz, ry, rm, rd, r_hour))
                .unwrap_or(0);
            let r_utc = r.local_time_ns - r_off_s as i128 * 1_000_000_000;
            (l.utc_time_ns, r_utc)
        } else {
            (l.local_time_ns, r.local_time_ns)
        }
    } else if !l.has_tz && r.has_tz {
        // Only rhs has TZ.  Apply rhs's named timezone to lhs.
        let named_tz = extract_named_tz(rhs);
        if !named_tz.is_empty() {
            let (ly, lm, ld) = if l.has_date {
                (l.year, l.month, l.day)
            } else {
                (r.year, r.month, r.day)
            };
            let l_hour = if l.has_time {
                ((l.local_time_ns / 3_600_000_000_000).abs()) as i64
            } else {
                0i64 // midnight
            };
            let l_off_s = parse_tz_offset_s(&tc_tz_suffix_ymdh(named_tz, ly, lm, ld, l_hour))
                .unwrap_or(0);
            let l_utc = l.local_time_ns - l_off_s as i128 * 1_000_000_000;
            (l_utc, r.utc_time_ns)
        } else {
            (l.local_time_ns, r.local_time_ns)
        }
    } else {
        // Neither has TZ: compare as wall-clock times.
        (l.local_time_ns, r.local_time_ns)
    };

    let l_epoch_ns = if l.has_date && r.has_date {
        temporal_epoch(l.year, l.month, l.day) as i128 * DAY_NS
    } else {
        0
    };
    let r_epoch_ns = if l.has_date && r.has_date {
        temporal_epoch(r.year, r.month, r.day) as i128 * DAY_NS
    } else {
        0
    };
    let total_diff = (r_epoch_ns + r_t) - (l_epoch_ns + l_t);
    if total_diff == 0 {
        return Some("PT0S".to_string());
    }
    let (h, min, s) = split_ns_to_hms(total_diff);
    Some(dur_fmt(0, 0, 0, h, min, s))
}

/// Extract the named timezone identifier from a datetime string such as
/// `"2017-10-29T00:00+02:00[Europe/Stockholm]"` → `"Europe/Stockholm"`.
/// Returns an empty string if there is no `[...]` suffix.
fn extract_named_tz(s: &str) -> &str {
    if let Some(open) = s.rfind('[') {
        let rest = &s[open + 1..];
        if let Some(close) = rest.find(']') {
            return &rest[..close];
        }
    }
    ""
}

// ── Epoch conversion ──────────────────────────────────────────────────────────

/// Convert Unix epoch seconds + sub-second nanoseconds to an ISO 8601 UTC
/// datetime string, as required by `datetime.fromepoch(seconds, nanoseconds)`.
/// Nanoseconds must be in 0..=999_999_999.
pub(crate) fn temporal_fromepoch_to_str(epoch_seconds: i64, nanoseconds: u32) -> String {
    // Split into Unix days and seconds within the day (always non-negative).
    let (unix_day, sec_of_day) = if epoch_seconds >= 0 {
        (epoch_seconds / 86400, (epoch_seconds % 86400) as u64)
    } else {
        let d = epoch_seconds / 86400;
        let s = epoch_seconds % 86400;
        if s < 0 {
            (d - 1, (s + 86400) as u64)
        } else {
            (d, s as u64)
        }
    };

    // Proleptic Gregorian day: temporal_epoch(1970, 1, 1) = 719163.
    let greg_day = unix_day + 719163;
    let (year, month, day) = temporal_from_epoch(greg_day);

    let hour = sec_of_day / 3600;
    let min = (sec_of_day % 3600) / 60;
    let sec = sec_of_day % 60;

    if nanoseconds == 0 {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            year, month, day, hour, min, sec
        )
    } else {
        // Trim trailing zeros (e.g. 987_000_000 → "987").
        let ns_str = format!("{:09}", nanoseconds)
            .trim_end_matches('0')
            .to_string();
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{}Z",
            year, month, day, hour, min, sec, ns_str
        )
    }
}

/// Convert Unix epoch milliseconds to an ISO 8601 UTC datetime string,
/// as required by `datetime.fromepochmillis(milliseconds)`.
pub(crate) fn temporal_fromepochmillis_to_str(epoch_ms: i64) -> String {
    let secs = epoch_ms / 1000;
    let ms_part = epoch_ms % 1000;
    // Handle negative values: ensure nanoseconds is always non-negative.
    let (secs_adj, ns) = if ms_part < 0 {
        (secs - 1, ((ms_part + 1000) * 1_000_000) as u32)
    } else {
        (secs, (ms_part * 1_000_000) as u32)
    };
    temporal_fromepoch_to_str(secs_adj, ns)
}
