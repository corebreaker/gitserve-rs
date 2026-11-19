//! gitserve: a minimal HTTP server exposing local bare Git repositories over the smart HTTP protocol.

pub mod archive;
mod ingest;
pub mod repository;
pub mod server;
pub mod store;
