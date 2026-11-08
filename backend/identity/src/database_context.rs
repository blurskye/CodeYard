pub trait DatabaseContext: Send + Sync {
    fn pool(&self) -> &codeyard_database::DbPool;
}
impl<T: DatabaseContext> DatabaseContext for std::sync::Arc<T> {
    fn pool(&self) -> &codeyard_database::DbPool {
        (**self).pool()
    }
}
