pub trait DatabaseContext: Send + Sync {
    fn pool(&self) -> &codeyard_db_core::DbPool;
}
impl<T: DatabaseContext> DatabaseContext for std::sync::Arc<T> {
    fn pool(&self) -> &codeyard_db_core::DbPool {
        (**self).pool()
    }
}
