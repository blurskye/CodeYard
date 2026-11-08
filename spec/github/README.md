# GitHub API descriptions

GitHub's own descriptions of its API, used to generate the `/api/v3` route table, the
coverage document and the GraphQL schema, and to check responses in tests.

| File | Source | Commit | License |
|---|---|---|---|
| `api.github.com.json.gz` | github/rest-api-description, `descriptions/api.github.com/api.github.com.json` | 755ee33b4a81d51f6f07a902cce26cbec3b822f2 | MIT, `LICENSE-rest-api-description.md` |
| `schema.docs.graphql` | github/docs, `src/graphql/data/fpt/schema.docs.graphql` | 5815d11586b0796688963a98c889680e60d72dc4 | MIT, `LICENSE-docs-code.md` |

The REST archive also contains the action-specific `components.schemas.webhook-*` payload
schemas. `tests/e2e/e2e-github.py` validates actual signed deliveries against these documents,
with local references only and OpenAPI nullable semantics. It does not fetch schemas at runtime.
