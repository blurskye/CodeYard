//! These directives describe schema elements, never executable query fields.
//! The normal directive-factory API also supports UNION/SCALAR locations, which
//! async-graphql 7.2.1's TypeDirective derive does not expose.
use async_graphql::{
    parser::types::Directive,
    registry::{MetaDirective, Registry},
    ContextDirective, CustomDirective, CustomDirectiveFactory, InputType, ServerError,
    ServerResult,
};
use std::borrow::Cow;

pub(crate) struct SchemaDirective(pub MetaDirective);

// Upstream's SDL exporter omits directive argument descriptions. Its introspection
// retains them; keep the offline export equally faithful using the same metadata.
pub(crate) fn restore_argument_descriptions(sdl: &str, definitions: &[SchemaDirective]) -> String {
    let mut output = String::new();
    for line in sdl.lines() {
        let directive = definitions
            .iter()
            .find(|d| line.starts_with(&format!("directive @{}(", d.0.name)));
        if let Some(SchemaDirective(directive)) = directive {
            output.push_str(&format!("directive @{}(\n", directive.name));
            for arg in directive.args.values() {
                if let Some(description) = &arg.description {
                    output.push_str(&format!(
                        "  {}\n",
                        serde_json::to_string(description).expect("directive description")
                    ));
                }
                output.push_str(&format!("  {}: {}", arg.name, arg.ty));
                if let Some(default) = &arg.default_value {
                    output.push_str(&format!(" = {default}"));
                }
                output.push('\n');
            }
            output.push(')');
            if directive.is_repeatable {
                output.push_str(" repeatable");
            }
            output.push_str(" on ");
            output.push_str(
                &directive
                    .locations
                    .iter()
                    .map(|location| location.to_value().to_string())
                    .collect::<Vec<_>>()
                    .join(" | "),
            );
        } else {
            output.push_str(line);
        }
        output.push('\n');
    }
    output
}

impl CustomDirectiveFactory for SchemaDirective {
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(self.0.name.clone())
    }

    fn register(&self, registry: &mut Registry) {
        registry.add_directive(MetaDirective {
            name: self.0.name.clone(),
            description: self.0.description.clone(),
            locations: self.0.locations.clone(),
            args: self.0.args.clone(),
            is_repeatable: self.0.is_repeatable,
            visible: self.0.visible,
            composable: self.0.composable.clone(),
        });
    }

    fn create(
        &self,
        _: &ContextDirective<'_>,
        _: &Directive,
    ) -> ServerResult<Box<dyn CustomDirective>> {
        Err(ServerError::new(
            "This directive is only valid in schema definitions",
            None,
        ))
    }
}
