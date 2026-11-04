//! Download formats project durable observations; archive members never stand in for executions.
use anyhow::Result;
use codeyard_db_actions::action_logs::Line;
use codeyard_db_actions::action_logs::Snapshot;
use std::collections::BTreeMap;
use std::io::Cursor;
use zip::write::SimpleFileOptions;
use zip::CompressionMethod;
use zip::ZipWriter;

fn text<'a>(lines: impl Iterator<Item = &'a Line>) -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in lines {
        writeln!(
            &mut bytes,
            "{} {}",
            line.created_at
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            line.line
        )
        .expect("writing to memory");
    }
    bytes
}
fn filename(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control() && !"/\\:<>\"|?*".contains(*c))
        .collect::<String>()
        .trim()
        .to_owned()
}
fn job_filename(name: &str) -> String {
    let utf16: Vec<_> = filename(name).encode_utf16().take(90).collect();
    String::from_utf16_lossy(&utf16)
        .trim()
        .trim_end_matches(['.', ' '])
        .to_owned()
}
fn archive_key(name: &str) -> String {
    name.to_uppercase()
}
fn usable(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    let numbered_device = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 240
        && !numbered_device
        && !matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
}
fn member(
    zip: &mut ZipWriter<Cursor<Vec<u8>>>,
    name: &str,
    body: &[u8],
    options: SimpleFileOptions,
) -> Result<()> {
    zip.start_file(name, options)?;
    zip.write_all(body)?;
    Ok(())
}

pub fn encode(snapshot: Snapshot) -> Result<(Vec<u8>, &'static str, String)> {
    if let Some(job) = snapshot.grant.job_id {
        let suffix = snapshot
            .grant
            .step_number
            .map(|step| format!("-step-{step}"))
            .unwrap_or_default();
        return Ok((
            text(snapshot.lines.iter()),
            "text/plain; charset=utf-8",
            format!("job-{job}{suffix}.txt"),
        ));
    }
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let now = chrono::Utc::now();
    use chrono::Timelike;
    let date = zip::DateTime::from_date_and_time(
        now.year().try_into()?,
        now.month() as u8,
        now.day() as u8,
        now.hour() as u8,
        now.minute() as u8,
        now.second().min(59) as u8,
    )?;
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(date)
        .unix_permissions(0o600);
    let mut counts = BTreeMap::<String, usize>::new();
    for job in &snapshot.grant.manifest.0.jobs {
        *counts
            .entry(archive_key(&job_filename(&job.name)))
            .or_default() += 1;
    }
    let mut reserved: std::collections::BTreeSet<String> = counts.keys().cloned().collect();
    // A job directory may not share a name with a file at the archive root.
    let mut roots: std::collections::BTreeSet<String> = snapshot
        .grant
        .manifest
        .0
        .jobs
        .iter()
        .enumerate()
        .map(|(index, job)| archive_key(&format!("{index}_{}.txt", job_filename(&job.name))))
        .collect();
    roots.insert(archive_key("native.log"));
    // One pass over the lines, so each job and step member reads only its own lines.
    let mut by_job: BTreeMap<_, Vec<&Line>> = BTreeMap::new();
    let mut by_step: BTreeMap<_, Vec<&Line>> = BTreeMap::new();
    for line in &snapshot.lines {
        by_job.entry(line.job_id).or_default().push(line);
        by_step
            .entry((line.job_id, line.step_number))
            .or_default()
            .push(line);
    }
    for (index, job) in snapshot.grant.manifest.0.jobs.iter().enumerate() {
        let base = job_filename(&job.name);
        let compatible = usable(&base)
            && counts.get(&archive_key(&base)) == Some(&1)
            && !roots.contains(&archive_key(&base));
        let directory = if compatible {
            base.clone()
        } else {
            // Do not let gh match the wrong member for two identically named jobs. Its
            // normal job-ID download fallback can retrieve each exact stream instead.
            let mut name = format!("__codeyard_job_{}__", job.id);
            while reserved.contains(&archive_key(&name)) || roots.contains(&archive_key(&name)) {
                name.push('_');
            }
            reserved.insert(archive_key(&name));
            name
        };
        let all = text(by_job.get(&Some(job.id)).into_iter().flatten().copied());
        let whole = if compatible {
            format!("{index}_{base}.txt")
        } else {
            format!("{directory}/job.log")
        };
        member(&mut zip, &whole, &all, options)?;
        for step in &job.steps {
            let name = filename(&step.name).chars().take(60).collect::<String>();
            let body = text(
                by_step
                    .get(&(Some(job.id), Some(step.number)))
                    .into_iter()
                    .flatten()
                    .copied(),
            );
            member(
                &mut zip,
                &format!("{directory}/{}_{}.txt", step.number, name),
                &body,
                options,
            )?;
        }
    }
    let unscoped = text(snapshot.lines.iter().filter(|line| line.job_id.is_none()));
    if !unscoped.is_empty() {
        member(&mut zip, "native.log", &unscoped, options)?;
    }
    Ok((
        zip.finish()?.into_inner(),
        "application/zip",
        format!("run-{}-logs.zip", snapshot.grant.run_id),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filenames_cannot_escape_the_archive() {
        for name in [
            "../../outside",
            "..\\..\\outside",
            "/absolute",
            ":bad\0name",
        ] {
            let name = job_filename(name);
            assert!(!name.contains(['/', '\\', ':', '\0']));
        }
        assert!(!usable(".."));
        assert!(!usable("CON"));
        assert_eq!(job_filename("Builder / unit: test"), "Builder  unit test");
    }
}
