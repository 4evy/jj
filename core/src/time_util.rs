// Copyright 2024 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Provides support for parsing and matching date ranges.

use chrono::DateTime;
use chrono::FixedOffset;
use chrono::Local;
use chrono::TimeZone;
use interim::DateError;
use interim::Dialect;
use interim::parse_date_string;
use thiserror::Error;

use crate::backend::MillisSinceEpoch;
use crate::backend::Timestamp;

/// Context needed to create a DatePattern during revset evaluation.
#[derive(Copy, Clone, Debug)]
pub enum DatePatternContext {
    /// Interpret date patterns using the local machine's time zone
    Local(DateTime<Local>),
    /// Interpret date patterns using any FixedOffset time zone
    Fixed(DateTime<FixedOffset>),
}

impl DatePatternContext {
    /// Parses a DatePattern from the given string and kind.
    pub fn parse_relative(
        &self,
        s: &str,
        kind: &str,
    ) -> Result<DatePattern, DatePatternParseError> {
        match *self {
            Self::Local(dt) => DatePattern::from_str_kind(s, kind, dt),
            Self::Fixed(dt) => DatePattern::from_str_kind(s, kind, dt),
        }
    }
}

impl From<DateTime<Local>> for DatePatternContext {
    fn from(value: DateTime<Local>) -> Self {
        Self::Local(value)
    }
}

impl From<DateTime<FixedOffset>> for DatePatternContext {
    fn from(value: DateTime<FixedOffset>) -> Self {
        Self::Fixed(value)
    }
}

/// Error occurred during date pattern parsing.
#[derive(Debug, Error)]
pub enum DatePatternParseError {
    /// Unknown pattern kind is specified.
    #[error("Invalid date pattern kind `{0}:`")]
    InvalidKind(String),
    /// Failed to parse timestamp.
    #[error(transparent)]
    ParseError(#[from] DateError),
}

/// Error occurred while parsing a date-time expression.
#[derive(Debug, Error)]
#[error("Couldn't understand `{input}` as a date and time")]
pub struct DateTimeExpressionParseError {
    input: String,
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
}

/// Represents an range of dates that may be matched against.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DatePattern {
    /// Represents all dates at or after the given instant.
    AtOrAfter(MillisSinceEpoch),
    /// Represents all dates before, but not including, the given instant.
    Before(MillisSinceEpoch),
}

impl DatePattern {
    /// Parses a string into a DatePattern.
    ///
    /// * `s` is the string to be parsed.
    ///
    /// * `kind` must be either "after" or "before". This determines whether the
    ///   pattern will match dates after or before the parsed date.
    ///
    /// * `now` is the user's current time. This is a [`DateTime<Tz>`] because
    ///   knowledge of offset changes is needed to correctly process relative
    ///   times like "today". For example, California entered DST on March 10,
    ///   2024, shifting clocks from UTC-8 to UTC-7 at 2:00 AM. If the pattern
    ///   "today" was parsed at noon on that day, it should be interpreted as
    ///   2024-03-10T00:00:00-08:00 even though the current offset is -07:00.
    pub fn from_str_kind<Tz: TimeZone>(
        s: &str,
        kind: &str,
        now: DateTime<Tz>,
    ) -> Result<Self, DatePatternParseError>
    where
        Tz::Offset: Copy,
    {
        let d =
            parse_date_string(s, now, Dialect::Us).map_err(DatePatternParseError::ParseError)?;
        let millis_since_epoch = MillisSinceEpoch(d.timestamp_millis());
        match kind {
            "after" => Ok(Self::AtOrAfter(millis_since_epoch)),
            "before" => Ok(Self::Before(millis_since_epoch)),
            kind => Err(DatePatternParseError::InvalidKind(kind.to_owned())),
        }
    }

    /// Determines whether a given timestamp is matched by the pattern.
    pub fn matches(&self, timestamp: &Timestamp) -> bool {
        match self {
            Self::AtOrAfter(earliest) => *earliest <= timestamp.timestamp,
            Self::Before(latest) => timestamp.timestamp < *latest,
        }
    }
}

// @TODO ideally we would have this unified with the other parsing code. However
// we use the interim crate which does not handle explicitly given time zone
// information
/// Parse a string with time zone information into a `Timestamp`
pub fn parse_datetime(s: &str) -> chrono::ParseResult<Timestamp> {
    Ok(Timestamp::from_datetime(
        DateTime::parse_from_rfc2822(s).or_else(|_| DateTime::parse_from_rfc3339(s))?,
    ))
}

/// Parses an exact or relative date-time expression relative to `now`.
///
/// Expressions without an explicit time zone are interpreted in `now`'s time
/// zone. For example, this accepts `now`, `yesterday 5pm`, and RFC 3339
/// timestamps.
pub fn parse_datetime_expression<Tz: TimeZone<Offset = FixedOffset>>(
    s: &str,
    now: DateTime<Tz>,
) -> Result<Timestamp, DateTimeExpressionParseError> {
    let s = s.trim();
    if let Ok(datetime) = DateTime::parse_from_rfc2822(s) {
        return Ok(Timestamp::from_datetime(datetime));
    }
    for result in [
        DateTime::parse_from_rfc3339(s),
        DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f %:z"),
        DateTime::parse_from_str(s, "%Y-%m-%d %H:%M %:z"),
    ] {
        match result {
            Ok(datetime) => return Ok(Timestamp::from_datetime(datetime)),
            // Do not let the relative parser normalize an invalid exact date
            Err(source)
                if matches!(
                    source.kind(),
                    chrono::format::ParseErrorKind::OutOfRange
                        | chrono::format::ParseErrorKind::Impossible
                ) =>
            {
                return Err(DateTimeExpressionParseError {
                    input: s.to_owned(),
                    source: source.into(),
                });
            }
            Err(_) => {}
        }
    }
    let datetime =
        parse_date_string(s, now, Dialect::Us).map_err(|source| DateTimeExpressionParseError {
            input: s.to_owned(),
            source: source.into(),
        })?;
    // Rebuild calendar dates through Chrono so their offsets reflect local
    // daylight-saving transitions rather than the offset at the UTC date
    let datetime = match now.timezone().from_local_datetime(&datetime.naive_local()) {
        chrono::LocalResult::Single(datetime) => datetime,
        chrono::LocalResult::Ambiguous(_, _) => datetime,
        chrono::LocalResult::None => {
            return Err(DateTimeExpressionParseError {
                input: s.to_owned(),
                source: DateError::MissingTime.into(),
            });
        }
    };
    Ok(Timestamp::from_datetime(datetime))
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    fn test_equal<Tz: TimeZone>(now: DateTime<Tz>, expression: &str, should_equal_time: &str)
    where
        Tz::Offset: Copy,
    {
        let expression = DatePattern::from_str_kind(expression, "after", now).unwrap();
        assert_eq!(
            expression,
            DatePattern::AtOrAfter(MillisSinceEpoch(
                DateTime::parse_from_rfc3339(should_equal_time)
                    .unwrap()
                    .timestamp_millis()
            ))
        );
    }

    #[test]
    fn test_date_pattern_parses_dates_without_times_as_the_date_at_local_midnight() {
        let now = DateTime::parse_from_rfc3339("2024-01-01T00:00:00-08:00").unwrap();
        test_equal(now, "2023-03-25", "2023-03-25T08:00:00Z");
        test_equal(now, "3/25/2023", "2023-03-25T08:00:00Z");
        test_equal(now, "3/25/23", "2023-03-25T08:00:00Z");
    }

    #[test]
    fn test_date_pattern_parses_dates_with_times_without_specifying_an_offset() {
        let now = DateTime::parse_from_rfc3339("2024-01-01T00:00:00-08:00").unwrap();
        test_equal(now, "2023-03-25T00:00:00", "2023-03-25T08:00:00Z");
        test_equal(now, "2023-03-25 00:00:00", "2023-03-25T08:00:00Z");
    }

    #[test]
    fn test_date_pattern_parses_dates_with_a_specified_offset() {
        let now = DateTime::parse_from_rfc3339("2024-01-01T00:00:00-08:00").unwrap();
        test_equal(
            now,
            "2023-03-25T00:00:00-05:00",
            "2023-03-25T00:00:00-05:00",
        );
    }

    #[test]
    fn test_date_pattern_parses_dates_with_the_z_offset() {
        let now = DateTime::parse_from_rfc3339("2024-01-01T00:00:00-08:00").unwrap();
        test_equal(now, "2023-03-25T00:00:00Z", "2023-03-25T00:00:00Z");
    }

    #[test]
    fn test_date_pattern_parses_relative_durations() {
        let now = DateTime::parse_from_rfc3339("2024-01-01T00:00:00-08:00").unwrap();
        test_equal(now, "2 hours ago", "2024-01-01T06:00:00Z");
        test_equal(now, "5 minutes", "2024-01-01T08:05:00Z");
        test_equal(now, "1 week ago", "2023-12-25T08:00:00Z");
        test_equal(now, "yesterday", "2023-12-31T08:00:00Z");
        test_equal(now, "tomorrow", "2024-01-02T08:00:00Z");
    }

    #[test]
    fn test_date_pattern_parses_relative_dates_with_times() {
        let now = DateTime::parse_from_rfc3339("2024-01-01T08:00:00-08:00").unwrap();
        test_equal(now, "yesterday 5pm", "2024-01-01T01:00:00Z");
        test_equal(now, "yesterday 10am", "2023-12-31T18:00:00Z");
        test_equal(now, "yesterday 10:30", "2023-12-31T18:30:00Z");
    }

    #[test]
    fn test_parse_datetime_non_sense_yields_error() {
        use chrono::format::ParseErrorKind;
        assert_matches!(
            parse_datetime("aaaaa").unwrap_err().kind(),
            ParseErrorKind::Invalid | ParseErrorKind::TooShort | ParseErrorKind::TooLong
        );
    }

    #[test]
    fn test_parse_datetime_human_readable() {
        // this is the example given in the help text for `jj metaedit
        // --author-timestamp`
        let timestamp = parse_datetime("2000-01-23T01:23:45-08:00").unwrap();
        let human_readable = parse_datetime("Sun, 23 Jan 2000 01:23:45 PST").unwrap();
        let human_readable_explicit = parse_datetime("Sun, 23 Jan 2000 01:23:45 -0800").unwrap();
        assert_eq!(timestamp, human_readable);
        assert_eq!(timestamp, human_readable_explicit);
    }

    #[test]
    fn test_parse_datetime_expression() {
        let now = DateTime::parse_from_rfc3339("2024-01-01T08:00:00-08:00").unwrap();
        let parse = |expression| parse_datetime_expression(expression, now).unwrap();

        assert_eq!(parse("now"), Timestamp::from_datetime(now));
        assert_eq!(
            parse("yesterday 5pm"),
            parse_datetime("2023-12-31T17:00:00-08:00").unwrap()
        );
        assert_eq!(
            parse("2023-03-25 01:30:45 -05:00"),
            parse_datetime("2023-03-25T01:30:45-05:00").unwrap()
        );
        for (expression, expected) in [
            ("32 hours", "2024-01-02T16:00:00-08:00"),
            ("42 days", "2024-02-12T08:00:00-08:00"),
            ("42 months", "2027-07-01T00:00:00-08:00"),
            ("25 years", "2049-01-01T00:00:00-08:00"),
            ("Sun, 23 Jan 2000 01:23:45 PST", "2000-01-23T01:23:45-08:00"),
        ] {
            assert_eq!(parse(expression), parse_datetime(expected).unwrap());
        }
    }

    #[test]
    fn test_parse_datetime_expression_preserves_exact_offset_and_precision() {
        let now = DateTime::parse_from_rfc3339("2024-01-01T08:00:00-08:00").unwrap();
        let expected = parse_datetime("2023-03-25T01:30:45.123+05:30").unwrap();
        for expression in [
            "2023-03-25 01:30:45.123 +05:30",
            "2023-03-25T01:30:45.123+05:30",
            " 2023-03-25T01:30:45.123+05:30 \n",
        ] {
            assert_eq!(
                parse_datetime_expression(expression, now).unwrap(),
                expected
            );
        }
        assert_eq!(
            parse_datetime_expression("2023-03-25 01:30 +05:30", now).unwrap(),
            parse_datetime("2023-03-25T01:30:00+05:30").unwrap()
        );
    }

    #[test]
    fn test_parse_datetime_expression_rejects_invalid_input() {
        let now = DateTime::parse_from_rfc3339("2024-01-01T08:00:00-08:00").unwrap();
        for expression in [
            "",
            " \n ",
            "not a date",
            "2023-02-29",
            "2024-02-30",
            "2024-13-01",
            "2024-01-01T12:00:00+25:00",
        ] {
            assert!(
                parse_datetime_expression(expression, now).is_err(),
                "accepted invalid input: {expression:?}"
            );
        }
    }
}
