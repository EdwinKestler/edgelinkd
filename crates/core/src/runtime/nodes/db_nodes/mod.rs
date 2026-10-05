//! Database client nodes. Compiled only with `nodes_postgres` and/or `nodes_redis`.

#[cfg(feature = "nodes_postgres")]
mod postgres;
#[cfg(feature = "nodes_redis")]
mod redis;
