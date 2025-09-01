use crate::Error;
use crate::ErrorContext;

pub fn parse_sequence_number(value: i64) -> Result<bitcoin::Sequence, Error> {
    /// The threshold that determines whether an expiry or exit delay should be parsed as a
    /// number of blocks or a number of seconds.
    ///
    /// - A value below 512 is considered a number of blocks.
    /// - A value over 512 is considered a number of seconds.
    const ARBITRARY_SEQUENCE_THRESHOLD: i64 = 512;

    let sequence = if value.is_negative() {
        return Err(Error::ad_hoc(format!("invalid sequence number: {value}")));
    } else if value < ARBITRARY_SEQUENCE_THRESHOLD {
        bitcoin::Sequence::from_height(value as u16)
    } else {
        bitcoin::Sequence::from_seconds_ceil(value as u32)
            .map_err(Error::ad_hoc)
            .with_context(|| format!("invalid sequence number in seconds: {value}"))?
    };

    Ok(sequence)
}
