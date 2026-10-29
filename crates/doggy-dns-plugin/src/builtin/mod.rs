use hickory_proto::rr::{LowerName, Name};

pub mod forward;
pub mod native;

/// Build the query name to hand a hickory resolver from a name that was already
/// parsed off the wire.
///
/// The resolver's `IntoName` is the trap here: `impl IntoName for &str` runs the
/// string through `Name::from_utf8`, which applies IDNA/STD3 validation, and
/// STD3 rejects interior underscores — only a leading `_` (the SRV-label
/// convention) is exempt. Handing it `name.to_string()` therefore turns a name
/// an upstream could answer, such as Nacos' `<service>.DEFAULT_GROUP.<ns>.<zone>`,
/// into a local SERVFAIL before any query leaves the process. Going through
/// `Name` selects the blanket `impl<T: Into<Name>> IntoName for T` instead, which
/// performs no re-derivation.
pub fn wire_query_name(name: &LowerName) -> Name {
    Name::from(name.clone())
}
