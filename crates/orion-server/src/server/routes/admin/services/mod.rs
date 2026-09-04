//! The activation gates, as functions over what they actually read.
//!
//! These lived in the route handlers and took `&AppState`, which made them
//! reachable through HTTP and nowhere else — you could not ask "would this
//! channel activate?" without building a request. Each one turns out to need a
//! single narrow dependency: a connector registry, a workflow repository, a
//! channel repository. Taking that instead of the whole state is the difference
//! between a service layer and a moved file.
//!
//! The rules themselves are unchanged, and the handlers still call them; what
//! changed is that something else can too.

pub(crate) mod channels;
pub(crate) mod plugins;
pub(crate) mod workflows;
