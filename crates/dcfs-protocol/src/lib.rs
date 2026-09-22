//! DCFS API protocol definitions.
//!
//! Versioned client/server request and response schemas.

pub mod errors;
pub mod name;
pub mod nodes;
pub mod versions;

pub use errors::*;
pub use name::*;
pub use nodes::*;
pub use versions::*;

/// API version.
pub const API_VERSION: &str = "v1";
