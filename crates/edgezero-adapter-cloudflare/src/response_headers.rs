use edgezero_core::http::HeaderMap;

/// Replace a body-generated default with the first valid application value,
/// then append additional values. Non-text values retain the adapter's skip policy.
pub(crate) fn copy_headers<E, F>(headers: &HeaderMap, mut write: F) -> Result<(), E>
where
    F: FnMut(&str, &str, bool) -> Result<(), E>,
{
    for name in headers.keys() {
        let mut replace = true;
        for value in headers.get_all(name) {
            if let Ok(text) = value.to_str() {
                write(name.as_str(), text, replace)?;
                replace = false;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::copy_headers;
    use edgezero_core::http::{HeaderMap, HeaderName, HeaderValue};

    #[test]
    fn replaces_defaults_and_preserves_separate_cookies() {
        let mut source = HeaderMap::new();
        source.append("set-cookie", HeaderValue::from_static("first=1; Path=/"));
        source.append("set-cookie", HeaderValue::from_static("second=2; Path=/"));
        source.insert("content-type", HeaderValue::from_static("text/plain"));
        let mut target = HeaderMap::new();
        target.insert(
            "content-type",
            HeaderValue::from_static("application/octet-stream"),
        );
        target.insert("set-cookie", HeaderValue::from_static("default=discard"));
        copy_headers(&source, |name, value, replace| {
            let parsed_name = name.parse::<HeaderName>().unwrap();
            let parsed_value = HeaderValue::from_str(value).unwrap();
            if replace {
                target.insert(parsed_name, parsed_value);
            } else {
                target.append(parsed_name, parsed_value);
            }
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(target, source);
        assert_eq!(target.get_all("set-cookie").iter().count(), 2);
    }

    #[test]
    fn first_valid_value_replaces_even_after_invalid_value() {
        let mut source = HeaderMap::new();
        source.append("x-test", HeaderValue::from_bytes(b"\xff").unwrap());
        source.append("x-test", HeaderValue::from_static("valid"));
        source.append("x-test", HeaderValue::from_static("next"));
        let mut writes = Vec::new();
        copy_headers(&source, |_, value, replace| {
            writes.push((value.to_owned(), replace));
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(
            writes,
            [("valid".to_owned(), true), ("next".to_owned(), false)]
        );
    }

    #[test]
    fn invalid_only_values_do_not_touch_generated_defaults() {
        let mut source = HeaderMap::new();
        source.append("x-test", HeaderValue::from_bytes(b"\xff").unwrap());
        copy_headers(&source, |_, _, _| -> Result<(), ()> {
            panic!("invalid text must not reach the sink")
        })
        .unwrap();
    }

    #[test]
    fn stops_at_sink_error() {
        let mut source = HeaderMap::new();
        source.append("set-cookie", HeaderValue::from_static("first=1"));
        source.append("set-cookie", HeaderValue::from_static("second=2"));
        let mut calls = 0_u32;
        let result = copy_headers(&source, |_, _, _| {
            calls += 1;
            Err("header rejected")
        });
        assert_eq!(result, Err("header rejected"));
        assert_eq!(calls, 1);
    }
}
