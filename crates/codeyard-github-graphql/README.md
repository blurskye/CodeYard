# GitHub GraphQL schema

`python3 scripts/github-graphql-types.py` reads `spec/github/schema.docs.graphql`
and regenerates `src/{objects,resolvers,inputs,enums,scalars,abstracts,schema,directives}.rs`.
Do not hand-edit those files. `--check` checks freshness, including formatting.
The other source files are maintained by hand.

The crate uses async-graphql 7.2.1's normal compile-time derives. Each object holds
an `Arc<dyn TypeResolver>`; each resolver trait has a typed method per SDL field
and a default `NOT_IMPLEMENTED` error. Applications implement only supported
methods and pass their own loaded state to the object constructor. There are no
application, database or authorization dependencies here. Schema-only directive
definitions use the normal directive-factory registration API, including locations
not exposed by upstream's TypeDirective macro.

`GitHubSchema` keeps registration and execution monomorphization in this crate.
The workspace disables this package's dev/test debug info; changing application
resolvers does not recompile the generated types. One-time metadata registration
uses a dedicated 32 MiB-stack thread; unoptimized full-SDL metadata exceeds the
normal Tokio/test worker stack.

Input wrappers preserve the original literal as well as the parsed, boxed value.
This keeps an SDL default such as `{}` from becoming `{state: null}` during
introspection, and breaks recursive input types without exposing another SDL type.

Upstream 7.2.1 hides explicitly registered orphan unions in introspection.
`introspection.rs` supplies only those unions, identified during generation,
from the actual derive registry. It honors aliases, fragments, variables and
selection directives. It also normalizes operation variable defaults through
`Request::parsed_query` before upstream directive pruning, which otherwise ignores
defaults in 7.2.1. Offline SDL export restores directive argument descriptions from
the same registered metadata. Reachable introspection and all data execution,
validation, null propagation and error paths remain upstream; no engine is patched
or vendored.

The library tests compare every exported type/field/argument with the vendored
SDL (including descriptions, defaults, deprecations and nullability), check input
literal preservation, and exercise orphan introspection. Router tests live in
`tests/repositories/graphql.rs` in the application.
