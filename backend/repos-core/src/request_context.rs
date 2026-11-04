pub trait RequestContext: Send + Sync {
    fn pool(&self) -> &codeyard_db_core::DbPool;
    fn cluster(&self) -> Option<&crate::context::NodeContext>;
}
impl<T: RequestContext> RequestContext for std::sync::Arc<T> {
    fn pool(&self) -> &codeyard_db_core::DbPool { (**self).pool() }
    fn cluster(&self) -> Option<&crate::context::NodeContext> { (**self).cluster() }
}
