//! NuGet compatibility classification, including metadata whose normalized version lost build data.
use anyhow::ensure;
use anyhow::Result;
use quick_xml::events::Event;
use quick_xml::Reader;
use serde_json::Value;

pub fn semver2(version: &str) -> bool {
    version.contains('+')
        || version
            .split_once('-')
            .is_some_and(|(_, pre)| pre.contains('.'))
}

pub fn metadata_semver2(name: &str, metadata: &Value) -> Result<bool> {
    if semver2(name)
        || metadata["dependencyGroups"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|group| group["dependencies"].as_array().into_iter().flatten())
            .filter_map(|dependency| dependency["range"].as_str())
            .any(|range| range.split([',', '[', ']', '(', ')', ' ']).any(semver2))
    {
        return Ok(true);
    }
    let Some(xml) = metadata["nuspec"].as_str() else {
        return Ok(false);
    };
    ensure!(xml.len() <= 1024 * 1024, "NuGet metadata exceeds its bound");
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<String> = Vec::new();
    let mut version = String::new();
    let mut nodes = 0;
    loop {
        match reader.read_event()? {
            Event::Start(tag) => {
                nodes += 1;
                ensure!(
                    nodes <= 16000 && stack.len() <= 32,
                    "NuGet XML exceeds its bound"
                );
                let name = tag.local_name();
                if name.as_ref() == "version" && stack.last().is_some_and(|name| name == "metadata")
                {
                    version.clear();
                }
                stack.push(name.as_ref().to_owned());
            }
            Event::End(_) => {
                if stack.last().is_some_and(|name| name == "version")
                    && stack
                        .iter()
                        .rev()
                        .nth(1)
                        .is_some_and(|name| name == "metadata")
                {
                    return Ok(semver2(version.trim()));
                }
                stack.pop();
            }
            Event::Text(text) if stack.last().is_some_and(|name| name == "version") => {
                version.push_str(&text)
            }
            Event::CData(text) if stack.last().is_some_and(|name| name == "version") => {
                version.push_str(&text)
            }
            Event::GeneralRef(reference) if stack.last().is_some_and(|name| name == "version") => {
                let escaped = format!("&{};", &*reference);
                version.push_str(&quick_xml::escape::unescape(&escaped)?);
            }
            Event::DocType(_) => anyhow::bail!("NuGet XML document types are unsupported"),
            Event::Eof => return Ok(false),
            _ => {}
        }
    }
}
