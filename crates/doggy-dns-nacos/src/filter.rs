use anyhow::anyhow;
use regex::Regex;

/// Which Nacos services this process answers for.
///
/// A name this filter rejects is never sent to Nacos, and the authority owns its
/// whole zone, so querying it yields authoritative NXDOMAIN rather than a
/// fall-through to another plugin.
#[derive(Debug)]
pub struct ServiceFilter {
    /// `None` is the built-in bare-application-name rule, checked byte-wise on
    /// the query path. A configured pattern keeps the regex crate's semantics.
    pattern: Option<Regex>,
}

/// Byte membership for the built-in rule. A table turns the three range
/// comparisons per byte into one load; the same set is the anchored
/// `[A-Za-z0-9_-]` class.
static BARE_APPLICATION_NAME_BYTES: [bool; 256] = {
    let mut table = [false; 256];
    let mut byte = 0;
    while byte < 256 {
        table[byte] = matches!(
            byte as u8,
            b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'_' | b'-'
        );
        byte += 1;
    }
    table
};

/// The built-in subscription rule: a bare application name — ASCII letters,
/// digits, `_`, `-`, and no dots.
///
/// This is the byte-wise equivalent of the anchored `[A-Za-z0-9_-]+` pattern.
/// It is spelled explicitly rather than as `\w` because the regex crate's
/// default `unicode` feature makes `\w` match CJK, which would quietly widen
/// the default to names an operator did not ask for. Configured patterns keep
/// that Unicode behaviour, deliberately.
#[inline]
fn is_bare_application_name(service_name: &str) -> bool {
    !service_name.is_empty()
        && service_name
            .bytes()
            .all(|byte| BARE_APPLICATION_NAME_BYTES[byte as usize])
}

impl ServiceFilter {
    /// Compile the configured pattern, or use the built-in rule when unset.
    ///
    /// The pattern is wrapped in whole-name anchors, so `order-service` selects
    /// that service and not `legacy-order-service`; a pattern that already spells
    /// its own anchors still compiles. Matching is case-sensitive, and the only
    /// production caller passes a name that `LowerName` already case-folded, so a
    /// pattern with capitals in it selects nothing.
    pub fn from_config(pattern: Option<&str>) -> anyhow::Result<Self> {
        let Some(configured) = pattern else {
            return Ok(Self { pattern: None });
        };
        let compiled = Regex::new(&format!("^(?:{configured})$"))
            .map_err(|e| anyhow!("service_pattern '{configured}' is not a valid regex: {e}"))?;
        Ok(Self {
            pattern: Some(compiled),
        })
    }

    /// True when `service_name` should be served.
    ///
    /// Case-sensitive. The only production caller passes a service label out of
    /// a `LowerName`, so that name is already lowercase here.
    #[inline]
    pub fn is_selected(&self, service_name: &str) -> bool {
        match &self.pattern {
            Some(pattern) => pattern.is_match(service_name),
            None => is_bare_application_name(service_name),
        }
    }
}
