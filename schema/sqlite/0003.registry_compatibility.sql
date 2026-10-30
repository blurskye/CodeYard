CREATE TABLE registry_namespaces (
    namespace TEXT PRIMARY KEY,
    owner_user_id INTEGER UNIQUE REFERENCES users(id) ON DELETE CASCADE,
    owner_org_id INTEGER UNIQUE REFERENCES organizations(id) ON DELETE CASCADE,
    CHECK ((owner_user_id IS NOT NULL) <> (owner_org_id IS NOT NULL))
);
