#[macro_use]
extern crate log;
extern crate serde;
extern crate serde_json;

use std::fmt;

use aw_models::TimeInterval;


pub mod datatype;

mod ast;
mod functions;
mod interpret;
mod lexer;
#[allow(
    clippy::match_single_binding,
    clippy::redundant_closure_call,
    unused_braces
)]
mod parser;

pub use crate::datatype::DataType;
pub use crate::interpret::VarEnv;

/// The whole of aw-query's coupling to storage.
///
/// Upstream takes `&aw_datastore::Datastore`, a concrete rusqlite-backed type,
/// which pins the query engine to SQLite even though the interpreter itself is
/// pure. Across the entire crate it makes exactly three calls -- get_buckets()
/// twice and get_events() once -- so a two-method trait frees it to run over
/// any store. Here that store is ClickHouse.
pub trait QuerySource {
    fn get_buckets(&self) -> Result<std::collections::HashMap<String, aw_models::Bucket>, QuerySourceError>;

    fn get_events(
        &self,
        bucket_id: &str,
        start: Option<chrono::DateTime<chrono::Utc>>,
        end: Option<chrono::DateTime<chrono::Utc>>,
        limit: Option<u64>,
    ) -> Result<Vec<aw_models::Event>, QuerySourceError>;
}

/// Mirrors the only two aw_datastore::DatastoreError variants the query engine
/// distinguishes: a missing bucket becomes QueryError::BucketNotFound, anything
/// else becomes BucketQueryError.
#[derive(Debug)]
pub enum QuerySourceError {
    NoSuchBucket(String),
    Other(String),
}

impl fmt::Display for QuerySourceError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            QuerySourceError::NoSuchBucket(b) => write!(f, "no such bucket: {b}"),
            QuerySourceError::Other(e) => write!(f, "{e}"),
        }
    }
}

// TODO: add line numbers to errors
// (works during lexing, but not during parsing I believe)

#[derive(Debug)]
pub enum QueryError {
    // Parser
    ParsingError(String),

    // Execution
    EmptyQuery(),
    VariableNotDefined(String),
    MathError(String),
    InvalidType(String),
    InvalidFunctionParameters(String),
    TimeIntervalError(String),
    BucketNotFound(String),
    BucketQueryError(String),
    RegexCompileError(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

pub fn query(code: &str, ti: &TimeInterval, ds: &dyn QuerySource) -> Result<DataType, QueryError> {
    let lexer = lexer::Lexer::new(code);
    let program = match parser::parse(lexer) {
        Ok(p) => p,
        Err(e) => {
            // TODO: Improve parsing error message
            warn!("ParsingError: {:?}", e);
            return Err(QueryError::ParsingError(format!("{e:?}")));
        }
    };
    interpret::interpret_prog(program, ti, ds)
}
