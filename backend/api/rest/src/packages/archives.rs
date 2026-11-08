//! Read bounded package metadata without extracting publisher-controlled paths.
use crate::packages::Error;
use crate::packages::Result;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::BTreeMap;
use std::io::Cursor;
use std::io::Read;

pub(super) const METADATA_LIMIT: usize = 1024 * 1024;

pub(super) fn bounded(reader: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::invalid())?;
    if bytes.len() > limit {
        return Err(Error::invalid());
    }
    Ok(bytes)
}

#[derive(Default)]
pub(super) struct Xml {
    pub name: String,
    pub text: String,
    pub attrs: BTreeMap<String, String>,
    pub children: Vec<Xml>,
}
impl Xml {
    pub fn child(&self, name: &str) -> Option<&Xml> {
        self.children.iter().find(|node| node.name == name)
    }
    pub fn value(&self, name: &str) -> &str {
        self.child(name).map_or("", |node| node.text.trim())
    }
}

pub(super) fn xml(bytes: &[u8]) -> Result<Xml> {
    // XML forbids NUL, and PostgreSQL cannot keep it in the JSON metadata built from this.
    if bytes.len() > METADATA_LIMIT || bytes.contains(&0) {
        return Err(Error::invalid());
    }
    let mut reader = Reader::from_reader(bytes);
    let mut stack = vec![Xml::default()];
    let mut count = 0;
    loop {
        let event = reader.read_event().map_err(|_| Error::invalid())?;
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(tag) | Event::Empty(tag) => {
                count += 1;
                if stack.len() > 32 || count > 16000 {
                    return Err(Error::invalid());
                }
                let mut node = Xml {
                    name: tag.local_name().as_ref().to_owned(),
                    ..Default::default()
                };
                for attr in tag.attributes() {
                    let attr = attr.map_err(|_| Error::invalid())?;
                    let key = attr.key.local_name().as_ref().to_owned();
                    let value = attr
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(|_| Error::invalid())?
                        .into_owned();
                    node.attrs.insert(key, value);
                }
                if empty {
                    stack
                        .last_mut()
                        .ok_or_else(Error::invalid)?
                        .children
                        .push(node);
                } else {
                    stack.push(node);
                }
            }
            Event::End(_) => {
                if stack.len() < 2 {
                    return Err(Error::invalid());
                }
                let node = stack.pop().ok_or_else(Error::invalid)?;
                stack
                    .last_mut()
                    .ok_or_else(Error::invalid)?
                    .children
                    .push(node);
            }
            Event::Text(text) => stack
                .last_mut()
                .ok_or_else(Error::invalid)?
                .text
                .push_str(&text),
            Event::CData(text) => stack
                .last_mut()
                .ok_or_else(Error::invalid)?
                .text
                .push_str(&text),
            Event::GeneralRef(reference) => {
                let escaped = format!("&{};", &*reference);
                let value = quick_xml::escape::unescape(&escaped).map_err(|_| Error::invalid())?;
                stack
                    .last_mut()
                    .ok_or_else(Error::invalid)?
                    .text
                    .push_str(&value);
            }
            Event::DocType(_) => return Err(Error::invalid()),
            Event::Eof => break,
            _ => {}
        }
    }
    let mut document = stack
        .pop()
        .filter(|_| stack.is_empty())
        .ok_or_else(Error::invalid)?;
    if document.children.len() != 1 || !document.text.trim().is_empty() {
        return Err(Error::invalid());
    }
    Ok(document.children.remove(0))
}

pub(super) fn nuspec(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut archive =
        codeyard_storage::zip_archives::open(bytes, 20_000).map_err(|_| Error::invalid())?;
    let mut result = None;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|_| Error::invalid())?;
        let name = entry.name().map_err(|_| Error::invalid())?;
        if !name.contains('/') && name.to_ascii_lowercase().ends_with(".nuspec") {
            if result.is_some() || entry.is_dir() {
                return Err(Error::invalid());
            }
            result = Some(bounded(entry, METADATA_LIMIT)?);
        }
    }
    result.ok_or_else(Error::invalid)
}

// YAML aliases can expand a small metadata file into gigabytes of cloned strings.
// Bound the expanded tree while deserializing; Ruby tags are data, never constructors.
struct YamlBudget {
    nodes: usize,
    bytes: usize,
}
struct YamlSeed<'a> {
    budget: &'a mut YamlBudget,
    depth: usize,
}
impl YamlSeed<'_> {
    fn charge<E: serde::de::Error>(&mut self, bytes: usize) -> std::result::Result<(), E> {
        self.budget.bytes = self
            .budget
            .bytes
            .checked_sub(bytes)
            .ok_or_else(|| E::custom("metadata exceeds expanded size limit"))?;
        Ok(())
    }
    fn child(&mut self) -> YamlSeed<'_> {
        YamlSeed {
            budget: self.budget,
            depth: self.depth + 1,
        }
    }
}
impl<'de> serde::de::DeserializeSeed<'de> for YamlSeed<'_> {
    type Value = serde_yaml::Value;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        use serde::de::Error as _;
        if self.depth > 32 {
            return Err(D::Error::custom("metadata nesting limit exceeded"));
        }
        self.budget.nodes = self
            .budget
            .nodes
            .checked_sub(1)
            .ok_or_else(|| D::Error::custom("metadata node limit exceeded"))?;
        deserializer.deserialize_any(self)
    }
}
impl<'de> serde::de::Visitor<'de> for YamlSeed<'_> {
    type Value = serde_yaml::Value;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("bounded package metadata")
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
        Ok(serde_yaml::Value::Null)
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<Self::Value, E> {
        Ok(serde_yaml::Value::Bool(v))
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
        Ok(serde_yaml::Value::Number(v.into()))
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
        Ok(serde_yaml::Value::Number(v.into()))
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
        Ok(serde_yaml::Value::Number(v.into()))
    }
    fn visit_str<E: serde::de::Error>(mut self, v: &str) -> std::result::Result<Self::Value, E> {
        self.charge::<E>(v.len())?;
        Ok(serde_yaml::Value::String(v.to_owned()))
    }
    fn visit_string<E: serde::de::Error>(
        mut self,
        v: String,
    ) -> std::result::Result<Self::Value, E> {
        self.charge::<E>(v.len())?;
        Ok(serde_yaml::Value::String(v))
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        mut self,
        mut seq: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(self.child())? {
            values.push(value);
        }
        Ok(serde_yaml::Value::Sequence(values))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(
        mut self,
        mut map: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        use serde::de::Error as _;
        let mut values = serde_yaml::Mapping::new();
        while let Some(key) = map.next_key_seed(self.child())? {
            let value = map.next_value_seed(self.child())?;
            if values.insert(key, value).is_some() {
                return Err(A::Error::custom("duplicate metadata key"));
            }
        }
        Ok(serde_yaml::Value::Mapping(values))
    }
    fn visit_enum<A: serde::de::EnumAccess<'de>>(
        mut self,
        data: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        use serde::de::VariantAccess as _;
        let (tag, variant) = data.variant::<String>()?;
        self.charge::<A::Error>(tag.len())?;
        variant.newtype_variant_seed(self.child())
    }
}
fn yaml(bytes: &[u8]) -> Result<serde_yaml::Value> {
    use serde::de::DeserializeSeed as _;
    if bytes.len() > METADATA_LIMIT {
        return Err(Error::invalid());
    }
    let mut documents = serde_yaml::Deserializer::from_slice(bytes);
    let document = documents.next().ok_or_else(Error::invalid)?;
    let mut budget = YamlBudget {
        nodes: 16000,
        bytes: METADATA_LIMIT,
    };
    let value = YamlSeed {
        budget: &mut budget,
        depth: 0,
    }
    .deserialize(document)
    .map_err(|_| Error::invalid())?;
    if documents.next().is_some() {
        return Err(Error::invalid());
    }
    Ok(value)
}

pub(super) fn gem_metadata(bytes: &[u8]) -> Result<serde_yaml::Value> {
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    let mut metadata = None;
    let mut data = false;
    for (index, entry) in archive.entries().map_err(|_| Error::invalid())?.enumerate() {
        if index >= 20000 {
            return Err(Error::invalid());
        }
        let entry = entry.map_err(|_| Error::invalid())?;
        let path = entry.path().map_err(|_| Error::invalid())?;
        if path == std::path::Path::new("metadata.gz") {
            if metadata.is_some() || !entry.header().entry_type().is_file() {
                return Err(Error::invalid());
            }
            let compressed = bounded(entry, METADATA_LIMIT)?;
            metadata = Some(bounded(
                flate2::read::GzDecoder::new(&compressed[..]),
                METADATA_LIMIT,
            )?);
        } else if path == std::path::Path::new("data.tar.gz") {
            if data || !entry.header().entry_type().is_file() {
                return Err(Error::invalid());
            }
            data = true;
        }
    }
    if !data {
        return Err(Error::invalid());
    }
    yaml(&metadata.ok_or_else(Error::invalid)?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn yaml_bounds_expanded_aliases_and_treats_ruby_tags_as_data() {
        let doc = crate::packages::archives::yaml(
            b"a: &copy !ruby/object:Gem::Version {version: '1.2'}\nb: *copy\n",
        )
        .unwrap();
        assert_eq!(doc["a"]["version"].as_str(), Some("1.2"));
        assert_eq!(doc["a"], doc["b"]);
        let bomb = format!(
            "big: &copy {}\nrefs: [{}]\n",
            "x".repeat(64_000),
            ["*copy"; 32].join(",")
        );
        assert!(crate::packages::archives::yaml(bomb.as_bytes()).is_err());
        assert!(crate::packages::archives::yaml(b"a: &copy [*copy]\n").is_err());
        assert!(crate::packages::archives::yaml(b"a: 1\n---\nb: 2\n").is_err());
    }
    #[test]
    fn xml_is_bounded_and_never_resolves_external_entities() {
        let xml = crate::packages::archives::xml(
            b"<package><metadata><id>A&amp;B</id><dependencies /></metadata></package>",
        )
        .unwrap();
        assert_eq!(xml.child("metadata").unwrap().value("id"), "A&B");
        // Attribute and text references resolve the predefined and character entities only.
        let xml = crate::packages::archives::xml(
            b"<package a=\"x&lt;&#65;\"><id>&#x42;&gt;</id><cdata><![CDATA[&amp;]]></cdata></package>",
        )
        .unwrap();
        assert_eq!(xml.attrs["a"], "x<A");
        assert_eq!(xml.value("id"), "B>");
        assert_eq!(xml.value("cdata"), "&amp;");
        assert!(crate::packages::archives::xml(b"<x>&custom;</x>").is_err());
        assert!(crate::packages::archives::xml(b"<x a=\"&custom;\"/>").is_err());
        assert!(crate::packages::archives::xml(b"<x>\xff</x>").is_err());
        assert!(
            crate::packages::archives::xml(b"<!DOCTYPE x SYSTEM 'file:///etc/passwd'><x/>")
                .is_err()
        );
        assert!(crate::packages::archives::xml(b"<a><b></a>").is_err());
        assert!(crate::packages::archives::xml(b"<a/><b/>").is_err());
    }
}
