#[allow(warnings)]
#[allow(clippy::all)]
mod generated {
    #[path = ""]
    pub mod ark {
        #[path = "ark.v1.rs"]
        pub mod v1;
    }
}

pub mod client;

mod error;
mod types;

pub use client::*;
pub use error::Error;

#[cfg(feature = "test-utils")]
pub mod test_utils;
