use serde_json::Value;

const KEYS: [&str; 15] = [
    "token", "email", "phone", "content", "username", "global_name", "name", "topic", "avatar", "icon", "banner",
    "bio", "url", "proxy_url", "filename",
];

/// Redacts personal string values (by key name) in place, recursively.
pub fn scrub(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                if val.is_string() && KEYS.contains(&k.as_str()) {
                    *val = Value::String("x".into());
                } else {
                    scrub(val);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(scrub),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scrub_redacts_nested() {
        let mut v = json!({"id": "1", "content": "hi", "author": {"email": "a@b", "name": "n", "id": "2"},
            "list": [{"content": "c", "id": "3"}]});
        scrub(&mut v);
        assert_eq!(v, json!({"id": "1", "content": "x", "author": {"email": "x", "name": "x", "id": "2"},
            "list": [{"content": "x", "id": "3"}]}));
    }
}
