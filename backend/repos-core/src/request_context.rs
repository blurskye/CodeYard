pub trait RequestContext: codeyard_identity::database_context::DatabaseContext {
    fn cluster(&self) -> Option<&crate::context::NodeContext>;
}
impl<T: RequestContext> RequestContext for std::sync::Arc<T> {
    fn cluster(&self) -> Option<&crate::context::NodeContext> { (**self).cluster() }
}
