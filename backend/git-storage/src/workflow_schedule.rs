use anyhow::{ensure, Context, Result};
use chrono::{TimeZone, Utc};
use croner::Cron;

pub fn parse(expression: &str) -> Result<Cron> {
    ensure!(expression.len() <= 256, "cron expression is too long");
    let fields: Vec<_> = expression.split_whitespace().collect();
    ensure!(fields.len() == 5, "cron requires five fields");
    let mut normalized = Vec::new();
    for (index, field) in fields.iter().enumerate() {
        ensure!(
            field.split(',').all(|term| !term.is_empty()),
            "empty cron term"
        );
        let mut field = field.to_ascii_uppercase();
        let names: &[&str] = match index {
            3 => &[
                "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
            ],
            4 => &["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"],
            _ => &[],
        };
        for (number, name) in names.iter().enumerate() {
            field = field.replace(name, &(number + usize::from(index == 3)).to_string());
        }
        ensure!(
            field
                .chars()
                .all(|ch| ch.is_ascii_digit() || "*,-/".contains(ch)),
            "unsupported cron syntax"
        );
        normalized.push(field);
    }
    let cron: Cron = normalized.join(" ").parse()?;
    let minutes: Cron = format!("{} * * * *", fields[0]).parse()?;
    let start = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .context("invalid cron date")?;
    let mut previous = None;
    let mut first = None;
    for minute in 0..60 {
        if minutes.is_time_matching(&(start + chrono::Duration::minutes(minute)))? {
            ensure!(
                previous.is_none_or(|previous| minute - previous >= 5),
                "cron interval is less than five minutes"
            );
            first.get_or_insert(minute);
            previous = Some(minute);
        }
    }
    if let (Some(first), Some(last)) = (first, previous) {
        if 60 + first - last < 5 {
            let hours: Cron = format!("0 {} * * *", fields[1]).parse()?;
            let selected: Vec<_> = (0..24)
                .map(|hour| hours.is_time_matching(&(start + chrono::Duration::hours(hour))))
                .collect::<std::result::Result<_, _>>()?;
            ensure!(
                !selected.windows(2).any(|pair| pair[0] && pair[1]),
                "cron interval is less than five minutes"
            );
            if selected[23] && selected[0] {
                let days: Cron =
                    format!("0 0 {} {} {}", normalized[2], normalized[3], normalized[4]).parse()?;
                let mut previous_day = false;
                for day in 0..=146097 {
                    let matches = days.is_time_matching(&(start + chrono::Duration::days(day)))?;
                    ensure!(
                        !(matches && previous_day),
                        "cron interval is less than five minutes"
                    );
                    previous_day = matches;
                }
            }
        }
    }
    Ok(cron)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schedules_use_posix_utc_and_at_least_five_minutes() {
        for expression in [
            "*/5 * * * *",
            "0 4 * * MON-FRI",
            "0,30 4 1 JAN MON",
            "0 0 * * 7",
        ] {
            assert!(parse(expression).is_ok(), "{expression}");
        }
        for expression in [
            "* * * * *",
            "1,2 * * * *",
            "59,0 * * * *",
            "0 0 0 * * *",
            "@daily",
            "0 0 L * *",
            "0 0 * * MON#2",
        ] {
            assert!(parse(expression).is_err(), "{expression}");
        }
        let cron = parse("0 4 1 JAN MON").unwrap();
        // POSIX matches either restricted day field, not their intersection.
        assert!(cron
            .is_time_matching(&Utc.with_ymd_and_hms(2026, 1, 1, 4, 0, 0).unwrap())
            .unwrap());
        assert!(cron
            .is_time_matching(&Utc.with_ymd_and_hms(2026, 1, 5, 4, 0, 0).unwrap())
            .unwrap());
    }
}
