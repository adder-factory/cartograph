//! Native filesystem corpus gate; publication and worker fault tests also run in `rocks_graph`.
#[path = "../test_support/dependency_ownership.rs"]
mod dependency_ownership;
#[path = "live_supervisor/native_corpus.rs"]
mod native_corpus;
