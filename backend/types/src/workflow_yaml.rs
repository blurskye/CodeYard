use crate::ci::MAX_CI_WORKFLOW_BYTES;
use anyhow::Result;
use serde_yaml::Value;
use std::collections::BTreeMap;
use tokio::sync::Semaphore;
pub static PARSING_SLOTS: Semaphore = Semaphore::const_new(2);

pub async fn parse_definitions(
    definitions: &BTreeMap<String, Vec<u8>>,
) -> Result<BTreeMap<String, Value>> {
    let permit = PARSING_SLOTS.acquire().await?;
    let definitions = definitions.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        definitions
            .into_iter()
            .map(|(path, bytes)| Ok((path, parse_yaml(&bytes)?)))
            .collect()
    })
    .await?
}

/// Parses workflow YAML. Reading a document copies every alias, so a small file could
/// expand to gigabytes. The expanded size is counted first, without building anything.
pub fn parse_yaml(content: &[u8]) -> Result<Value> {
    parse_yaml_with_limit(content, 8 * MAX_CI_WORKFLOW_BYTES)
}

pub fn parse_yaml_with_limit(content: &[u8], max_expanded_bytes: usize) -> Result<Value> {
    use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess};
    use std::cell::Cell;

    #[derive(Clone, Copy)]
    struct Measure<'a>(&'a Cell<usize>, usize);

    impl Measure<'_> {
        fn add<E: de::Error>(self, bytes: usize) -> Result<(), E> {
            let total = self.0.get().saturating_add(bytes + 16);
            self.0.set(total);
            if total > self.1 {
                return Err(E::custom("the workflow is too large once aliases expand"));
            }
            Ok(())
        }
    }

    impl<'de> DeserializeSeed<'de> for Measure<'_> {
        type Value = ();
        fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
            deserializer.deserialize_any(self)
        }
    }

    impl<'de> de::Visitor<'de> for Measure<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a YAML value")
        }
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
            self.add(1)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
            self.add(8)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
            self.add(8)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
            self.add(8)
        }
        fn visit_str<E: de::Error>(self, text: &str) -> Result<(), E> {
            self.add(text.len())
        }
        fn visit_unit<E: de::Error>(self) -> Result<(), E> {
            self.add(0)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            self.add(0)?;
            while seq.next_element_seed(self)?.is_some() {}
            Ok(())
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            self.add(0)?;
            while map.next_key_seed(self)?.is_some() {
                map.next_value_seed(self)?;
            }
            Ok(())
        }
        fn visit_enum<A: de::EnumAccess<'de>>(self, data: A) -> Result<(), A::Error> {
            use de::VariantAccess;
            // The tag name is copied into the value with every alias, so it counts too.
            let (name, variant): (String, _) = data.variant()?;
            self.add(name.len())?;
            variant.newtype_variant_seed(self)
        }
    }

    let used = Cell::new(0);
    Measure(&used, max_expanded_bytes)
        .deserialize(serde_yaml::Deserializer::from_slice(content))?;
    Ok(serde_yaml::from_slice(content)?)
}
