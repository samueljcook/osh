use sqlx::postgres::{PgTypeInfo, PgValueFormat, PgValueRef};
use sqlx::{Decode, Postgres, Type, error::BoxDynError};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

/// A `timestamptz` value that a query read, as an instant in UTC.
///
/// Read a `timestamptz` column as this type in [`portable_query_as`](super::portable_query_as),
/// [`portable_query_scalar`](super::portable_query_scalar), and any other statement that uses
/// the simple query protocol. PostgreSQL answers such a statement in text, and it prints a
/// `timestamptz` in the session's time zone, without a fraction when the value has a whole
/// number of seconds. The text decoder of SQLx 0.9 for `time::OffsetDateTime` fails on a value
/// without a fraction and on a value in a time zone that is behind UTC or that has an offset
/// with minutes, and for a value with a fraction in a time zone that is ahead of UTC by whole
/// hours it returns a time that is wrong by the offset. This type decodes every such text to the
/// correct instant.
///
/// The session's `DateStyle` output format must be `ISO`, which is PostgreSQL's default. The
/// decoder fails, and never guesses, on the text of another output format, on `infinity` and
/// `-infinity`, and on a year that `time::OffsetDateTime` cannot hold. The failure of a portable
/// query is a [`DatabaseError`](super::DatabaseError), and no failure contains the text.
///
/// ```rust,no_run
/// # async fn example(pool: &paranoid::db::Pool) -> Result<(), Box<dyn std::error::Error>> {
/// use paranoid::db::{PgTimestamptz, portable_query_scalar};
///
/// let created_at: time::OffsetDateTime =
///     portable_query_scalar::<PgTimestamptz>("SELECT created_at FROM accounts WHERE id = $1")
///         .bind(7_i64)
///         .fetch_one(pool)
///         .await?
///         .0;
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgTimestamptz(pub OffsetDateTime);

impl From<PgTimestamptz> for OffsetDateTime {
	fn from(value: PgTimestamptz) -> Self {
		value.0
	}
}

impl Type<Postgres> for PgTimestamptz {
	fn type_info() -> PgTypeInfo {
		<OffsetDateTime as Type<Postgres>>::type_info()
	}
}

impl Decode<'_, Postgres> for PgTimestamptz {
	fn decode(value: PgValueRef<'_>) -> Result<Self, BoxDynError> {
		match value.format() {
			PgValueFormat::Binary => <OffsetDateTime as Decode<Postgres>>::decode(value).map(Self),
			PgValueFormat::Text => parse_timestamptz_text(value.as_str()?)
				.map(Self)
				.map_err(Into::into),
		}
	}
}

/// Parses the text that PostgreSQL prints for a `timestamptz` under the `ISO` output format:
/// `YYYY-MM-DD HH:MM:SS`, an optional fraction of one to six digits, the offset of the session's
/// time zone as `+HH`, `+HH:MM`, or `+HH:MM:SS` (or with `-`), and ` BC` for a year before 1 AD.
/// The result is the instant in UTC. The error never contains the text.
fn parse_timestamptz_text(text: &str) -> Result<OffsetDateTime, &'static str> {
	const MALFORMED: &str = "the text is not a timestamptz in PostgreSQL's ISO output format";
	const OUT_OF_RANGE: &str = "the timestamptz is outside the range of time::OffsetDateTime";

	if text == "infinity" || text == "-infinity" {
		return Err("an infinite timestamptz has no time::OffsetDateTime value");
	}
	let (text, is_before_christ) = match text.strip_suffix(" BC") {
		Some(text) => (text, true),
		None => (text, false),
	};
	let (date_text, time_text) = text.split_once(' ').ok_or(MALFORMED)?;

	let mut date_fields = date_text.split('-');
	let (Some(year), Some(month), Some(day), None) = (
		date_fields.next(),
		date_fields.next(),
		date_fields.next(),
		date_fields.next(),
	) else {
		return Err(MALFORMED);
	};
	if year.len() < 4 {
		return Err(MALFORMED);
	}
	let year = parse_digits(year).ok_or(MALFORMED)?;
	let month = parse_two_digits(month).ok_or(MALFORMED)?;
	let day = parse_two_digits(day).ok_or(MALFORMED)?;

	let offset_start = time_text.find(['+', '-']).ok_or(MALFORMED)?;
	let (clock_text, offset_text) = time_text.split_at(offset_start);
	let (clock_text, microsecond) = match clock_text.split_once('.') {
		Some((clock_text, fraction)) => {
			if fraction.is_empty() || fraction.len() > 6 {
				return Err(MALFORMED);
			}
			let digits = parse_digits(fraction).ok_or(MALFORMED)?;
			let scale = 10_u32.pow(6 - u32::try_from(fraction.len()).map_err(|_| MALFORMED)?);
			(clock_text, digits * scale)
		}
		None => (clock_text, 0),
	};
	let [hour, minute, second] = parse_colon_fields::<3>(clock_text).ok_or(MALFORMED)?;

	let (offset_sign, offset_fields) = match offset_text.split_at(1) {
		("+", fields) => (1_i8, fields),
		("-", fields) => (-1_i8, fields),
		_ => return Err(MALFORMED),
	};
	let [offset_hour, offset_minute, offset_second] = match offset_fields.matches(':').count() {
		0 => {
			let [hour] = parse_colon_fields::<1>(offset_fields).ok_or(MALFORMED)?;
			[hour, 0, 0]
		}
		1 => {
			let [hour, minute] = parse_colon_fields::<2>(offset_fields).ok_or(MALFORMED)?;
			[hour, minute, 0]
		}
		2 => parse_colon_fields::<3>(offset_fields).ok_or(MALFORMED)?,
		_ => return Err(MALFORMED),
	};

	// PostgreSQL has no year 0: 1 BC is the year before 1 AD, which is year 0 in the
	// proleptic Gregorian calendar of the `time` crate.
	let year = i32::try_from(year).map_err(|_| OUT_OF_RANGE)?;
	let year = if is_before_christ { 1 - year } else { year };
	let month = Month::try_from(month).map_err(|_| MALFORMED)?;
	let date = Date::from_calendar_date(year, month, day).map_err(|_| OUT_OF_RANGE)?;
	let time = Time::from_hms_micro(hour, minute, second, microsecond).map_err(|_| MALFORMED)?;
	let signed = |field: u8| i8::try_from(field).map(|field| field * offset_sign);
	let offset = UtcOffset::from_hms(
		signed(offset_hour).map_err(|_| MALFORMED)?,
		signed(offset_minute).map_err(|_| MALFORMED)?,
		signed(offset_second).map_err(|_| MALFORMED)?,
	)
	.map_err(|_| MALFORMED)?;
	PrimitiveDateTime::new(date, time)
		.assume_offset(offset)
		.checked_to_offset(UtcOffset::UTC)
		.ok_or(OUT_OF_RANGE)
}

/// The number that one to nine ASCII digits write, or `None` for any other text.
fn parse_digits(text: &str) -> Option<u32> {
	if text.is_empty() || text.len() > 9 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
		return None;
	}
	text.parse().ok()
}

fn parse_two_digits(text: &str) -> Option<u8> {
	if text.len() != 2 {
		return None;
	}
	parse_digits(text).and_then(|number| u8::try_from(number).ok())
}

/// Exactly `N` two-digit fields separated by colons.
fn parse_colon_fields<const N: usize>(text: &str) -> Option<[u8; N]> {
	let mut fields = [0_u8; N];
	let mut parts = text.split(':');
	for field in &mut fields {
		*field = parse_two_digits(parts.next()?)?;
	}
	parts.next().is_none().then_some(fields)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The instant at the given calendar date and time of day in UTC.
	fn utc(
		(year, month, day): (i32, u8, u8),
		(hour, minute, second, microsecond): (u8, u8, u8, u32),
	) -> OffsetDateTime {
		let date = Date::from_calendar_date(year, Month::try_from(month).expect("month"), day)
			.expect("date");
		let time = Time::from_hms_micro(hour, minute, second, microsecond).expect("time");
		PrimitiveDateTime::new(date, time).assume_utc()
	}

	#[test]
	fn whole_seconds_fractions_and_every_offset_form_give_the_instant_in_utc() {
		let midnight = (0, 0, 0, 0);
		for (text, expected) in [
			("2026-09-15 00:00:00+00", utc((2026, 9, 15), midnight)),
			(
				"2026-09-15 00:00:00.5+00",
				utc((2026, 9, 15), (0, 0, 0, 500_000)),
			),
			(
				"2026-09-15 00:00:00.000001+00",
				utc((2026, 9, 15), (0, 0, 0, 1)),
			),
			(
				"2026-09-15 02:00:00.5+02",
				utc((2026, 9, 15), (0, 0, 0, 500_000)),
			),
			("2026-09-14 20:00:00-04", utc((2026, 9, 15), midnight)),
			(
				"2026-09-15 05:30:00.25+05:30",
				utc((2026, 9, 15), (0, 0, 0, 250_000)),
			),
			("1880-01-01 00:19:32+00:19:32", utc((1880, 1, 1), midnight)),
			("0001-01-01 00:00:00+00", utc((1, 1, 1), midnight)),
			(
				"0001-12-31 23:59:59.999999+00 BC",
				utc((0, 12, 31), (23, 59, 59, 999_999)),
			),
			(
				"0044-03-15 12:00:00+00 BC",
				utc((-43, 3, 15), (12, 0, 0, 0)),
			),
			("4714-11-24 00:00:00+00 BC", utc((-4713, 11, 24), midnight)),
		] {
			assert_eq!(parse_timestamptz_text(text), Ok(expected), "{text}");
		}
	}

	#[test]
	fn text_in_another_form_is_an_error() {
		for text in [
			"",
			"infinity",
			"-infinity",
			"2026-09-15",
			"2026-09-15 00:00:00",
			"2026-09-15T00:00:00+00",
			"2026-09-15 00:00:00 UTC",
			"15/09/2026 00:00:00.5 UTC",
			"Tue Sep 15 00:00:00 2026 UTC",
			"15.09.2026 00:00:00 UTC",
			"2026-09-15 00:00:00.+00",
			"2026-09-15 00:00:00.1234567+00",
			"2026-09-15 00:00:00+0",
			"2026-09-15 00:00:00+00:0",
			"2026-09-15 00:00:00+00:00:00:00",
			"2026-09-15 0:00:00+00",
			"2026-9-15 00:00:00+00",
			"026-09-15 00:00:00+00",
			"2026-13-15 00:00:00+00",
			"2026-02-30 00:00:00+00",
			"2026-09-15 24:00:00+00",
			"2026-09-15 00:00:00+26",
			"+2026-09-15 00:00:00+00",
			"2026-09-15 00:00:00+00 AD",
			"２０２６-09-15 00:00:00+00",
		] {
			assert!(parse_timestamptz_text(text).is_err(), "{text}");
		}
	}

	#[test]
	fn a_year_that_time_cannot_hold_is_out_of_range() {
		assert_eq!(
			parse_timestamptz_text("294276-12-31 23:59:59.999999+00"),
			Err("the timestamptz is outside the range of time::OffsetDateTime")
		);
	}
}
