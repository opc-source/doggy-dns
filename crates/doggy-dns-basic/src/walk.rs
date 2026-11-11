//! One recursion over a TOML value tree, shared by env expansion and secret
//! redaction so both spell paths the same way.

use anyhow::Result;

/// Visit every string in `value` depth-first, with its dotted path
/// (`table.key`, `array[0]`) and, for a table entry, its key.
///
/// The visitor may rewrite the string in place; a non-string value is
/// descended into, never visited. An array item has no key of its own
/// (`None`), so a sensitive table key does not reach into the strings of an
/// array it names — the same shape redaction had before the walk was shared.
/// The first visitor error stops the walk and propagates.
pub fn walk_strings(
    value: &mut toml::Value,
    path: &str,
    visit: &mut impl FnMut(&str, Option<&str>, &mut String) -> Result<()>,
) -> Result<()> {
    walk(value, path, None, visit)
}

fn walk(
    value: &mut toml::Value,
    path: &str,
    key: Option<&str>,
    visit: &mut impl FnMut(&str, Option<&str>, &mut String) -> Result<()>,
) -> Result<()> {
    match value {
        toml::Value::String(text) => visit(path, key, text),
        toml::Value::Table(table) => {
            for (child_key, child) in table.iter_mut() {
                let child_path = if path.is_empty() {
                    child_key.clone()
                } else {
                    format!("{path}.{child_key}")
                };
                walk(child, &child_path, Some(child_key.as_str()), visit)?;
            }
            Ok(())
        }
        toml::Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                walk(item, &format!("{path}[{index}]"), None, visit)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
