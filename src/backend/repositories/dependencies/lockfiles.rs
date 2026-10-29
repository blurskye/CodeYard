//! Read resolved versions without invoking package managers or following external files.
use super::npm::{MAX_LOCK_BYTES, MAX_PACKAGES};
use crate::backend::repositories::dependency_graph::Package;
use anyhow::{ensure, Context, Result};
use serde_json::Value;

pub const FILES: &[&str] = &[
    "Cargo.lock",
    "go.mod",
    "requirements.txt",
    "poetry.lock",
    "Pipfile.lock",
    "uv.lock",
    "Gemfile.lock",
    "composer.lock",
    "pom.xml",
    "gradle.lockfile",
    "packages.lock.json",
    "pubspec.lock",
    "mix.lock",
    "yarn.lock",
    "pnpm-lock.yaml",
];

fn add(
    out: &mut Vec<Package>,
    path: &str,
    ecosystem: &str,
    name: &str,
    version: &str,
) -> Result<()> {
    let name = if ecosystem == "pypi" {
        crate::backend::vulnerabilities::python_name(name)
    } else if ecosystem == "nuget" {
        name.to_ascii_lowercase()
    } else {
        name.to_owned()
    };
    ensure!(
        !name.is_empty() && name.len() <= 512 && version.len() <= 256 && !version.is_empty(),
        "Invalid locked dependency"
    );
    ensure!(
        !name
            .chars()
            .chain(version.chars())
            .any(|c| c.is_control() || c.is_whitespace()),
        "Invalid locked dependency"
    );
    let encode = crate::webui::pages::repos::ref_path::encode_path;
    out.push(Package {
        manifest: path.into(),
        ecosystem: ecosystem.into(),
        name: name.clone(),
        version: version.into(),
        package_url: format!(
            "pkg:{ecosystem}/{}@{}",
            encode(&name),
            crate::webui::encode_component(version)
        ),
        scope: "unknown".into(),
    });
    ensure!(out.len() <= MAX_PACKAGES, "Too many locked dependencies");
    Ok(())
}
fn toml(text: &str) -> Result<Value> {
    Ok(serde_json::to_value(toml::from_str::<toml::Value>(text)?)?)
}
fn entries<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    value[key].as_array().context("Missing locked packages")
}
fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key].as_str().context("Missing locked package field")
}
fn cargo(path: &str, text: &str, out: &mut Vec<Package>) -> Result<()> {
    for p in entries(&toml(text)?, "package")? {
        if p["source"]
            .as_str()
            .is_some_and(|s| s.starts_with("registry+"))
        {
            add(out, path, "cargo", field(p, "name")?, field(p, "version")?)?;
        }
    }
    Ok(())
}
fn python_toml(path: &str, text: &str, out: &mut Vec<Package>) -> Result<()> {
    for p in entries(&toml(text)?, "package")? {
        let registry = if path.ends_with("uv.lock") {
            p["source"]["registry"].is_string()
        } else {
            p.get("source").is_none() || p["source"]["type"] == "legacy"
        };
        if registry {
            add(out, path, "pypi", field(p, "name")?, field(p, "version")?)?;
        }
    }
    Ok(())
}
fn pipfile(path: &str, text: &str, out: &mut Vec<Package>) -> Result<()> {
    let value: Value = serde_json::from_str(text)?;
    for section in ["default", "develop"] {
        for (name, p) in value[section]
            .as_object()
            .context("Missing Pipfile section")?
        {
            if let Some(version) = p["version"].as_str().and_then(|s| s.strip_prefix("==")) {
                add(out, path, "pypi", name, version)?;
            }
        }
    }
    Ok(())
}
fn requirements(path: &str, text: &str, out: &mut Vec<Package>) -> Result<()> {
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with("--hash=") {
            continue;
        }
        // Unresolved requirements such as `sphinx>=7` are not installed versions and cannot be
        // matched safely, so they are left out, as unresolved Maven versions are. Whitespace may
        // surround the operator, and pip options and a line continuation follow the requirement
        // after whitespace.
        let requirement: String = line
            .split(';')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .take_while(|token| !token.starts_with('-'))
            .filter(|token| *token != "\\")
            .collect();
        let Some((name, version)) = requirement
            .split_once("==")
            .filter(|(_, version)| !version.contains(['*', '=', '<', '>', ',']))
        else {
            continue;
        };
        add(
            out,
            path,
            "pypi",
            name.split('[').next().unwrap_or(name),
            version,
        )?;
    }
    Ok(())
}
fn go_mod(path: &str, text: &str, out: &mut Vec<Package>) -> Result<()> {
    let mut block = false;
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        // Go separates a directive from its arguments with any whitespace.
        let directive = line
            .split(|c: char| c.is_whitespace() || c == '(')
            .next()
            .unwrap_or("");
        let rest = line[directive.len()..].trim();
        if matches!(directive, "replace" | "exclude") {
            anyhow::bail!("Go replacements and exclusions need a submitted snapshot");
        }
        let line = if block {
            if line == ")" {
                block = false;
                continue;
            }
            line
        } else if directive == "require" {
            if rest == "(" {
                block = true;
                continue;
            }
            rest
        } else {
            continue;
        };
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }
        ensure!(parts.len() == 2, "Invalid Go requirement");
        add(
            out,
            path,
            "golang",
            parts[0],
            parts[1].trim_start_matches('v'),
        )?;
    }
    Ok(())
}
fn gems(path: &str, text: &str, out: &mut Vec<Package>) -> Result<()> {
    let mut registry = false;
    for line in text.lines() {
        if !line.starts_with(' ') && !line.is_empty() {
            registry = line == "GEM";
        }
        if registry && line.starts_with("    ") && !line.starts_with("     ") {
            if let Some((name, version)) = line.trim().split_once(" (") {
                let version = version.trim_end_matches(')');
                // Platform suffixes are RubyGems build metadata, not part of the release version.
                add(
                    out,
                    path,
                    "gem",
                    name,
                    version.split('-').next().unwrap_or(version),
                )?;
            }
        }
    }
    Ok(())
}
fn composer(path: &str, text: &str, out: &mut Vec<Package>) -> Result<()> {
    let value: Value = serde_json::from_str(text)?;
    for section in ["packages", "packages-dev"] {
        if let Some(packages) = value[section].as_array() {
            for p in packages {
                let version = field(p, "version")?.trim_start_matches('v');
                if !version.starts_with("dev-") && !version.ends_with("-dev") {
                    add(out, path, "composer", field(p, "name")?, version)?;
                }
            }
        }
    }
    Ok(())
}
/// Versions may come from `<properties>`, the project's own coordinates or
/// `<dependencyManagement>`. Ranges and anything that names a parent POM's values stay unread.
fn maven(path: &str, text: &str, out: &mut Vec<Package>) -> Result<()> {
    use quick_xml::{events::Event, Reader};
    use std::collections::BTreeMap;
    const DECLARED: [&str; 3] = ["project", "dependencies", "dependency"];
    const MANAGED: [&str; 4] = [
        "project",
        "dependencyManagement",
        "dependencies",
        "dependency",
    ];
    let mut reader = Reader::from_str(text);
    let mut stack = Vec::<String>::new();
    let mut properties = BTreeMap::<String, String>::new();
    let mut current = BTreeMap::<String, String>::new();
    let mut declared = Vec::new();
    let mut managed = Vec::new();
    loop {
        match reader.read_event()? {
            Event::Start(tag) => {
                stack.push(String::from_utf8(tag.local_name().as_ref().to_vec())?);
                if stack == DECLARED || stack == MANAGED {
                    current.clear();
                }
            }
            Event::Text(value) => {
                let value = value.decode()?.trim().to_owned();
                match stack
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .as_slice()
                {
                    ["project", "properties", name] => {
                        properties.insert((*name).to_owned(), value);
                    }
                    ["project", field @ ("version" | "groupId")] => {
                        properties.insert(format!("project.{field}"), value);
                    }
                    ["project", "parent", field @ ("version" | "groupId")] => {
                        properties.insert(format!("project.parent.{field}"), value);
                    }
                    ["project", "dependencies", "dependency", field]
                    | ["project", "dependencyManagement", "dependencies", "dependency", field] => {
                        current.insert((*field).to_owned(), value);
                    }
                    _ => {}
                }
            }
            Event::End(_) => {
                if stack == DECLARED {
                    declared.push(std::mem::take(&mut current));
                } else if stack == MANAGED {
                    managed.push(std::mem::take(&mut current));
                }
                ensure!(
                    declared.len() + managed.len() <= MAX_PACKAGES,
                    "Too many locked dependencies"
                );
                stack.pop();
            }
            Event::DocType(_) => anyhow::bail!("XML document types are not supported"),
            Event::Eof => break,
            _ => {}
        }
        ensure!(stack.len() <= 64, "Dependency XML is too deep");
    }
    // A child inherits its parent's group and version when it does not set its own.
    for field in ["groupId", "version"] {
        if let Some(value) = properties.get(&format!("project.parent.{field}")).cloned() {
            properties
                .entry(format!("project.{field}"))
                .or_insert(value);
        }
    }
    let resolve = |value: &str| -> Option<String> {
        let mut value = value.to_owned();
        for _ in 0..8 {
            let Some(start) = value.find("${") else {
                return Some(value);
            };
            let end = start + value[start..].find('}')?;
            let replacement = properties.get(&value[start + 2..end])?.clone();
            value.replace_range(start..=end, &replacement);
        }
        (!value.contains("${")).then_some(value)
    };
    // Maven manages a dependency by its group, artifact, type and classifier.
    let coordinates = |dependency: &BTreeMap<String, String>| -> Option<[String; 4]> {
        let field =
            |key: &str, default: &str| resolve(dependency.get(key).map_or(default, String::as_str));
        Some([
            resolve(dependency.get("groupId")?.as_str())?,
            resolve(dependency.get("artifactId")?.as_str())?,
            field("type", "jar")?,
            field("classifier", "")?,
        ])
    };
    let mut versions = std::collections::HashMap::new();
    for entry in &managed {
        if let (Some(key), Some(version)) = (coordinates(entry), entry.get("version")) {
            versions.entry(key).or_insert(version);
        }
    }
    for dependency in &declared {
        let Some(key) = coordinates(dependency) else {
            continue;
        };
        let version = dependency
            .get("version")
            .or_else(|| versions.get(&key).copied());
        let [group, name, ..] = key;
        let Some(version) = version.and_then(|version| resolve(version.as_str())) else {
            continue;
        };
        if !version.contains(['$', '[', ']', '(', ')', ','])
            && !matches!(version.as_str(), "LATEST" | "RELEASE")
        {
            add(out, path, "maven", &format!("{group}/{name}"), &version)?;
        }
    }
    Ok(())
}
fn gradle(path: &str, text: &str, out: &mut Vec<Package>) -> Result<()> {
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("empty="))
    {
        let (coordinates, _) = line.split_once('=').context("Invalid Gradle lock entry")?;
        let parts: Vec<_> = coordinates.split(':').collect();
        ensure!(parts.len() == 3, "Invalid Maven coordinates");
        add(
            out,
            path,
            "maven",
            &format!("{}/{}", parts[0], parts[1]),
            parts[2],
        )?;
    }
    Ok(())
}
fn nuget(path: &str, text: &str, out: &mut Vec<Package>) -> Result<()> {
    let value: Value = serde_json::from_str(text)?;
    for packages in value["dependencies"]
        .as_object()
        .context("Missing NuGet dependencies")?
        .values()
    {
        for (name, p) in packages.as_object().context("Invalid NuGet target")? {
            if p["type"] != "Project" {
                add(out, path, "nuget", name, field(p, "resolved")?)?;
            }
        }
    }
    Ok(())
}
fn pubspec(path: &str, text: &str, out: &mut Vec<Package>) -> Result<()> {
    let value: Value = serde_yaml::from_str(text)?;
    for (name, p) in value["packages"]
        .as_object()
        .context("Missing Pub dependencies")?
    {
        if p["source"] == "hosted" {
            add(out, path, "pub", name, field(p, "version")?)?;
        }
    }
    Ok(())
}
fn mix(path: &str, text: &str, out: &mut Vec<Package>) -> Result<()> {
    for line in text.lines() {
        if let Some((_, tuple)) = line.split_once("{:hex,") {
            let mut parts = tuple.split(',').map(str::trim);
            let name = parts
                .next()
                .context("Missing Hex name")?
                .trim_start_matches(':')
                .trim_matches('"');
            let version = parts
                .next()
                .context("Missing Hex version")?
                .trim_matches('"');
            add(out, path, "hex", name, version)?;
        }
    }
    Ok(())
}
fn yarn(path: &str, text: &str, out: &mut Vec<Package>) -> Result<()> {
    let mut names = Vec::<String>::new();
    for line in text.lines() {
        if !line.starts_with(' ') && line.ends_with(':') {
            names = line
                .trim_end_matches(':')
                .split(", ")
                .filter_map(|s| {
                    let s = s.trim_matches('"');
                    let (name, _) = s.rsplit_once('@')?;
                    // An npm alias installs the package it names.
                    let name = name.split_once("@npm:").map_or(name, |(_, target)| target);
                    Some(name.to_owned())
                })
                .collect();
        } else if let Some(version) = line
            .trim()
            .strip_prefix("version ")
            .or_else(|| line.trim().strip_prefix("version:"))
        {
            for name in &names {
                add(out, path, "npm", name, version.trim().trim_matches('"'))?;
            }
        }
    }
    Ok(())
}
fn pnpm(path: &str, text: &str, out: &mut Vec<Package>) -> Result<()> {
    let value: Value = serde_yaml::from_str(text)?;
    for key in value["packages"]
        .as_object()
        .context("Missing pnpm packages")?
        .keys()
    {
        let key = key.trim_start_matches('/').split('(').next().unwrap_or(key);
        // Newer keys put '@' between the name and version, and a scope also starts with '@'. Older
        // keys end in "/version".
        let pair = key
            .rsplit_once('@')
            .filter(|(name, _)| !name.is_empty())
            .or_else(|| key.rsplit_once('/'));
        if let Some((name, version)) = pair {
            add(out, path, "npm", name, version)?;
        }
    }
    Ok(())
}

pub fn parse(path: &str, bytes: &[u8]) -> Result<Vec<Package>> {
    ensure!(
        bytes.len() <= MAX_LOCK_BYTES,
        "Dependency file is too large"
    );
    let text = std::str::from_utf8(bytes)?;
    let mut out = Vec::new();
    match path.rsplit('/').next().unwrap_or(path) {
        "Cargo.lock" => cargo(path, text, &mut out)?,
        "go.mod" => go_mod(path, text, &mut out)?,
        "requirements.txt" => requirements(path, text, &mut out)?,
        "poetry.lock" | "uv.lock" => python_toml(path, text, &mut out)?,
        "Pipfile.lock" => pipfile(path, text, &mut out)?,
        "Gemfile.lock" => gems(path, text, &mut out)?,
        "composer.lock" => composer(path, text, &mut out)?,
        "pom.xml" => maven(path, text, &mut out)?,
        "gradle.lockfile" => gradle(path, text, &mut out)?,
        "packages.lock.json" => nuget(path, text, &mut out)?,
        "pubspec.lock" => pubspec(path, text, &mut out)?,
        "mix.lock" => mix(path, text, &mut out)?,
        "yarn.lock" => yarn(path, text, &mut out)?,
        "pnpm-lock.yaml" => pnpm(path, text, &mut out)?,
        _ => anyhow::bail!("Unsupported dependency file"),
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_lockfile_samples() {
        for (path, bytes, ecosystem, name, version) in [
            (
                "Cargo.lock",
                include_bytes!("fixtures/Cargo.lock").as_slice(),
                "cargo",
                "serde",
                "1.0.228",
            ),
            (
                "go.mod",
                include_bytes!("fixtures/go.mod").as_slice(),
                "golang",
                "golang.org/x/text",
                "0.14.0",
            ),
            (
                "requirements.txt",
                include_bytes!("fixtures/requirements.txt").as_slice(),
                "pypi",
                "django",
                "4.2.0",
            ),
            (
                "poetry.lock",
                include_bytes!("fixtures/poetry.lock").as_slice(),
                "pypi",
                "django",
                "4.2.0",
            ),
            (
                "uv.lock",
                include_bytes!("fixtures/uv.lock").as_slice(),
                "pypi",
                "django",
                "4.2.0",
            ),
            (
                "Pipfile.lock",
                include_bytes!("fixtures/Pipfile.lock").as_slice(),
                "pypi",
                "django",
                "4.2.0",
            ),
            (
                "Gemfile.lock",
                include_bytes!("fixtures/Gemfile.lock").as_slice(),
                "gem",
                "rack",
                "2.2.8",
            ),
            (
                "composer.lock",
                include_bytes!("fixtures/composer.lock").as_slice(),
                "composer",
                "symfony/http-foundation",
                "6.4.0",
            ),
            (
                "pom.xml",
                include_bytes!("fixtures/pom.xml").as_slice(),
                "maven",
                "org.example/core",
                "1.0",
            ),
            (
                "gradle.lockfile",
                include_bytes!("fixtures/gradle.lockfile").as_slice(),
                "maven",
                "org.example/core",
                "1.0",
            ),
            (
                "packages.lock.json",
                include_bytes!("fixtures/packages.lock.json").as_slice(),
                "nuget",
                "newtonsoft.json",
                "13.0.1",
            ),
            (
                "pubspec.lock",
                include_bytes!("fixtures/pubspec.lock").as_slice(),
                "pub",
                "http",
                "1.1.0",
            ),
            (
                "mix.lock",
                include_bytes!("fixtures/mix.lock").as_slice(),
                "hex",
                "plug",
                "1.15.0",
            ),
            (
                "yarn.lock",
                include_bytes!("fixtures/yarn.lock").as_slice(),
                "npm",
                "lodash",
                "4.17.20",
            ),
            (
                "pnpm-lock.yaml",
                include_bytes!("fixtures/pnpm-lock.yaml").as_slice(),
                "npm",
                "lodash",
                "4.17.20",
            ),
        ] {
            let parsed = parse(path, bytes).unwrap_or_else(|e| panic!("{path}: {e}"));
            assert!(
                parsed
                    .iter()
                    .any(|p| p.ecosystem == ecosystem && p.name == name && p.version == version),
                "{path}: {parsed:?}"
            );
        }
    }
    #[test]
    fn maven_properties_and_unresolved_versions() {
        let pom = br#"<project><groupId>org.example</groupId><version>2.1</version>
<properties><jackson.version>2.15.0</jackson.version></properties>
<dependencyManagement><dependencies><dependency><groupId>org.managed</groupId><artifactId>lib</artifactId><version>${project.version}</version></dependency></dependencies></dependencyManagement>
<dependencies>
<dependency><groupId>com.fasterxml.jackson.core</groupId><artifactId>jackson-databind</artifactId><version>${jackson.version}</version></dependency>
<dependency><groupId>org.managed</groupId><artifactId>lib</artifactId></dependency>
<dependency><groupId>${project.groupId}</groupId><artifactId>sibling</artifactId><version>${project.version}</version></dependency>
<dependency><groupId>g</groupId><artifactId>range</artifactId><version>[1.0,2.0)</version></dependency>
</dependencies></project>"#;
        let parsed = parse("services/api/pom.xml", pom).unwrap();
        let found: Vec<_> = parsed
            .iter()
            .map(|p| (p.manifest.as_str(), p.name.as_str(), p.version.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                (
                    "services/api/pom.xml",
                    "com.fasterxml.jackson.core/jackson-databind",
                    "2.15.0"
                ),
                ("services/api/pom.xml", "org.example/sibling", "2.1"),
                ("services/api/pom.xml", "org.managed/lib", "2.1"),
            ]
        );
    }
    #[test]
    fn spaced_pip_pins_and_scoped_pnpm_v5_keys_are_read() {
        let requirements = parse("requirements.txt", b"Django == 4.2.0 --hash=sha256:x\n").unwrap();
        assert_eq!(
            (
                requirements[0].name.as_str(),
                requirements[0].version.as_str()
            ),
            ("django", "4.2.0")
        );
        let tabbed = parse("requirements.txt", b"Django==4.2.0\t--hash=sha256:x \\\n").unwrap();
        assert_eq!(tabbed[0].version, "4.2.0");
        let lock = b"lockfileVersion: '5.4'\npackages:\n  /@scope/pkg/1.2.3:\n    resolution: {}\n";
        let parsed = parse("pnpm-lock.yaml", lock).unwrap();
        assert_eq!(
            (parsed[0].name.as_str(), parsed[0].version.as_str()),
            ("@scope/pkg", "1.2.3")
        );
    }
    #[test]
    fn aliases_tabbed_go_requirements_and_managed_classifiers() {
        let lock = b"\"alias@npm:ansi-regex@^5.0.0\":\n  version \"5.0.0\"\n";
        assert_eq!(parse("yarn.lock", lock).unwrap()[0].name, "ansi-regex");
        let parsed = parse(
            "go.mod",
            b"module m\n\nrequire\tgolang.org/x/text v0.14.0\n",
        )
        .unwrap();
        assert_eq!(
            (parsed[0].name.as_str(), parsed[0].version.as_str()),
            ("golang.org/x/text", "0.14.0")
        );
        let pom = br#"<project><dependencyManagement><dependencies>
<dependency><groupId>g</groupId><artifactId>a</artifactId><version>2</version><type>test-jar</type><classifier>tests</classifier></dependency>
<dependency><groupId>g</groupId><artifactId>a</artifactId><version>1</version></dependency>
</dependencies></dependencyManagement>
<dependencies><dependency><groupId>g</groupId><artifactId>a</artifactId></dependency></dependencies></project>"#;
        assert_eq!(parse("pom.xml", pom).unwrap()[0].version, "1");
    }
    #[test]
    fn no_unresolved_versions_or_external_xml() {
        assert!(parse("requirements.txt", b"django>=4\nsphinx==7.*\n")
            .unwrap()
            .is_empty());
        assert!(parse("go.mod", b"replace example.com/x => ../x\n").is_err());
        assert!(parse("pom.xml", b"<!DOCTYPE project><project/>").is_err());
        assert!(parse("pom.xml",b"<project><dependencies><dependency><groupId>g</groupId><artifactId>a</artifactId><version>${v}</version></dependency></dependencies></project>").unwrap().is_empty());
        assert!(parse("Cargo.lock", &vec![b' '; MAX_LOCK_BYTES + 1]).is_err());
    }
}
