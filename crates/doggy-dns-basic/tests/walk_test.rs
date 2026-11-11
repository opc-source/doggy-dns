#![allow(clippy::disallowed_methods)]

use doggy_dns_basic::walk::walk_strings;

#[test]
fn visits_every_string_with_its_dotted_path() {
    let mut value: toml::Value = toml::from_str(
        r#"
        top = "a"
        [server]
        name = "b"
        [[plugins]]
        kind = "c"
        [[plugins]]
        kind = "d"
        "#,
    )
    .expect("valid toml");
    let mut seen = Vec::new();
    walk_strings(&mut value, "", &mut |path, _key, text| {
        seen.push((path.to_string(), text.clone()));
        Ok(())
    })
    .expect("walk succeeds");
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("plugins[0].kind".to_string(), "c".to_string()),
            ("plugins[1].kind".to_string(), "d".to_string()),
            ("server.name".to_string(), "b".to_string()),
            ("top".to_string(), "a".to_string()),
        ]
    );
}

#[test]
fn table_keys_are_passed_and_array_items_have_none() {
    let mut value: toml::Value =
        toml::from_str("password = \"p\"\nlist = [\"x\"]").expect("valid toml");
    let mut keys = Vec::new();
    walk_strings(&mut value, "", &mut |_path, key, _text| {
        keys.push(key.map(str::to_string));
        Ok(())
    })
    .expect("walk succeeds");
    keys.sort();
    assert_eq!(keys, vec![None, Some("password".to_string())]);
}

#[test]
fn visitor_rewrites_strings_in_place() {
    let mut value = toml::Value::String("x".to_string());
    walk_strings(&mut value, "", &mut |_path, _key, text| {
        text.push('!');
        Ok(())
    })
    .expect("walk succeeds");
    assert_eq!(value.as_str(), Some("x!"));
}

#[test]
fn an_error_stops_the_walk_and_propagates() {
    let mut value: toml::Value = toml::from_str("a = \"1\"\nb = \"2\"").expect("valid toml");
    let mut calls = 0;
    let result = walk_strings(&mut value, "", &mut |_path, _key, _text| {
        calls += 1;
        anyhow::bail!("stop")
    });
    assert!(result.is_err());
    assert_eq!(calls, 1);
}

#[test]
fn non_string_values_are_descended_not_visited() {
    let mut value: toml::Value =
        toml::from_str("n = 1\nf = 1.5\nb = true\nd = 2024-01-01\ns = \"ok\"").expect("valid toml");
    let mut seen = Vec::new();
    walk_strings(&mut value, "", &mut |path, _key, _text| {
        seen.push(path.to_string());
        Ok(())
    })
    .expect("walk succeeds");
    assert_eq!(seen, vec!["s".to_string()]);
}
