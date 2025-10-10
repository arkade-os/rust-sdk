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

pub fn parse_sequence_number(value: i64) -> Result<bitcoin::Sequence, Error> {
    /// The threshold that determines whether an expiry or exit delay should be parsed as a
    /// number of blocks or a number of seconds.
    ///
    /// - A value below 512 is considered a number of blocks.
    /// - A value over 512 is considered a number of seconds.
    const ARBITRARY_SEQUENCE_THRESHOLD: i64 = 512;

    let sequence = if value.is_negative() {
        return Err(Error::conversion(format!(
            "invalid sequence number: {value}"
        )));
    } else if value < ARBITRARY_SEQUENCE_THRESHOLD {
        bitcoin::Sequence::from_height(value as u16)
    } else {
        bitcoin::Sequence::from_seconds_ceil(value as u32).map_err(Error::conversion)?
    };

    Ok(sequence)
}
