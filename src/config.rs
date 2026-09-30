use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct Registration {
    pub id: String,
    pub url: String,
    pub as_token: String,
    pub hs_token: String,
    pub sender_localpart: String,
    pub namespaces: Namespaces,
    #[serde(
        rename = "de.matrix.org.msc2409.ephemeral",
        alias = "receive_ephemeral",
        alias = "ephemeral",
        default
    )]
    pub receive_ephemeral: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Namespaces {
    pub users: Vec<Namespace>,
    pub aliases: Vec<Namespace>,
    pub rooms: Vec<Namespace>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Namespace {
    pub exclusive: bool,
    pub regex: String,
}

pub(crate) fn generate_token() -> String {
    use rand::distr::SampleString;
    rand::distr::Alphanumeric.sample_string(&mut rand::rng(), 64)
}

#[must_use]
pub fn generate_registration(url: &str) -> Registration {
    Registration {
        id: "jmap-bridge".to_owned(),
        url: url.to_owned(),
        as_token: generate_token(),
        hs_token: generate_token(),
        sender_localpart: "_jmap_bot".to_owned(),
        namespaces: Namespaces {
            users: vec![Namespace {
                exclusive: true,
                regex: "@_jmap_.*".to_owned(),
            }],
            aliases: vec![],
            rooms: vec![],
        },
        receive_ephemeral: true,
    }
}

/// Rejected `--backfill-window` / `BACKFILL_WINDOW` value.
#[derive(Debug, thiserror::Error)]
pub enum BackfillWindowError {
    /// The span did not parse as an ISO 8601 or jiff "friendly" duration.
    #[error(
        "invalid backfill window {input:?}: {source}. Expected a duration like \
         `1w`, `30d`, `6mo` or `1 year` (note: `1m` is one MINUTE — a month is `1mo`)"
    )]
    Parse {
        /// The operator-supplied text.
        input: String,
        /// The underlying jiff parse failure.
        source: jiff::Error,
    },
    /// A negative or zero span, which would select no mail at all.
    #[error("backfill window {0:?} must be a positive duration")]
    NotPositive(String),
}

/// Parse an operator-supplied backfill window.
///
/// Accepts jiff's friendly format (`1w`, `30d`, `6mo`, `1 year`, `3 days 4 hours`)
/// and ISO 8601 durations (`P30D`). The sentinel `all` — and an empty value, which
/// is how an unset environment variable usually arrives — mean "no limit" and yield
/// `Ok(None)`, so backfill walks the whole mailbox as it always has.
///
/// # Errors
///
/// Returns [`BackfillWindowError`] if the value does not parse, or if it is zero or
/// negative (which would silently bridge nothing).
pub fn parse_backfill_window(input: &str) -> Result<Option<jiff::Span>, BackfillWindowError> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("all") {
        return Ok(None);
    }
    let span: jiff::Span = trimmed
        .parse()
        .map_err(|source| BackfillWindowError::Parse {
            input: trimmed.to_owned(),
            source,
        })?;
    if span.is_negative() || span.is_zero() {
        return Err(BackfillWindowError::NotPositive(trimmed.to_owned()));
    }
    Ok(Some(span))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod backfill_window_tests {
    use super::parse_backfill_window;

    #[test]
    fn friendly_and_iso_spellings_parse() {
        for input in ["1w", "1 week", "30d", "6mo", "1 year", "P30D", "3d 4h"] {
            assert!(
                parse_backfill_window(input).unwrap().is_some(),
                "{input} should parse"
            );
        }
    }

    #[test]
    fn unset_and_all_mean_no_limit() {
        for input in ["", "   ", "all", "ALL"] {
            assert!(parse_backfill_window(input).unwrap().is_none(), "{input:?}");
        }
    }

    #[test]
    fn rejects_nonsense_and_non_positive() {
        for input in ["last tuesday", "1 fortnight", "-7d", "0d"] {
            assert!(parse_backfill_window(input).is_err(), "{input:?}");
        }
    }

    /// `1m` is one MINUTE in this format, not one month — the single most likely
    /// operator mistake, so pin the behaviour rather than let it drift.
    #[test]
    fn m_is_minutes_and_mo_is_months() {
        let minute = parse_backfill_window("1m").unwrap().unwrap();
        assert_eq!(minute.get_minutes(), 1);
        assert_eq!(minute.get_months(), 0);

        let month = parse_backfill_window("1mo").unwrap().unwrap();
        assert_eq!(month.get_months(), 1);
        assert_eq!(month.get_minutes(), 0);
    }
}
