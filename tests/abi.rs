#[cfg(test)]
mod test {

    use compiler::abi::{classify_decode, classify_encode, DecodeBits, EncodeRequest, ErrorStash, HandleTable, PrimitiveBytes, WireValue};

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn bits(s: &str) -> u64 { u64::from_str_radix(s, 16).unwrap() }

    /* Corpus wire-value forms, null | bool | {"i": decimal} | {"f": num|"inf"|"-inf"} | {"s": text} | {"raw": hex} | [..] | {"d": [[k, v], ..]}. */
    fn wire_value(v: &serde_json::Value) -> WireValue {
        match v {
            serde_json::Value::Null => WireValue::None,
            serde_json::Value::Bool(b) => WireValue::Bool(*b),
            serde_json::Value::Array(items) => WireValue::List(items.iter().map(wire_value).collect()),
            serde_json::Value::Object(m) => {
                if let Some(i) = m.get("i") {
                    WireValue::Int(i.as_str().unwrap().parse().unwrap())
                } else if let Some(f) = m.get("f") {
                    WireValue::Float(match f {
                        serde_json::Value::String(s) if s == "inf" => f64::INFINITY,
                        serde_json::Value::String(s) if s == "-inf" => f64::NEG_INFINITY,
                        other => other.as_f64().unwrap(),
                    })
                } else if let Some(s) = m.get("s") {
                    WireValue::Bytes(s.as_str().unwrap().as_bytes().to_vec())
                } else if let Some(h) = m.get("raw") {
                    WireValue::Raw(unhex(h.as_str().unwrap()))
                } else if let Some(d) = m.get("d") {
                    WireValue::Dict(d.as_array().unwrap().iter().map(|pair| {
                        let kv = pair.as_array().unwrap();
                        (wire_value(&kv[0]), wire_value(&kv[1]))
                    }).collect())
                } else {
                    panic!("unknown wire-value form: {v}")
                }
            }
            _ => panic!("unknown wire-value form: {v}"),
        }
    }

    fn check_encode(case: &serde_json::Value) {
        let tag = case["tag"].as_u64().unwrap() as u32;
        let payload = unhex(case["hex"].as_str().unwrap());
        let expect = &case["expect"];
        match (classify_encode(tag, &payload), expect) {
            (EncodeRequest::Invalid, e) if e == "invalid" => {}
            (EncodeRequest::Direct(b), e) if e.get("direct").is_some() => assert_eq!(b, bits(e["direct"].as_str().unwrap()), "direct bits mismatch on: {case}"),
            // Every NaN gets an identity of its own, so only its being a float NaN is fixed.
            (EncodeRequest::Direct(b), e) if e.get("nan").is_some() => assert!(f64::from_bits(b).is_nan() && (b & 0x7FFC_0000_0000_0000) != 0x7FFC_0000_0000_0000, "nan mismatch on: {case}"),
            (EncodeRequest::AllocStr(s), e) if e.get("str").is_some() => assert_eq!(s, e["str"].as_str().unwrap(), "str mismatch on: {case}"),
            (EncodeRequest::AllocBytes(b), e) if e.get("bytes").is_some() => assert_eq!(b, unhex(e["bytes"].as_str().unwrap()), "bytes mismatch on: {case}"),
            (EncodeRequest::AllocInt(i), e) if e.get("boxed").is_some() => assert_eq!(i, e["boxed"].as_str().unwrap().parse::<i128>().unwrap(), "boxed int mismatch on: {case}"),
            (EncodeRequest::Composite(w), e) if e.get("composite").is_some() => assert_eq!(w, wire_value(&e["composite"]), "composite mismatch on: {case}"),
            _ => panic!("encode outcome kind mismatch on: {case}"),
        }
    }

    fn check_decode(case: &serde_json::Value) {
        let val_bits = bits(case["bits"].as_str().unwrap());
        let expect = &case["expect"];
        match (classify_decode(val_bits), expect) {
            (DecodeBits::Heap, e) if e == "heap" => {}
            (DecodeBits::Invalid, e) if e == "invalid" => {}
            (DecodeBits::Primitive { tag, bytes }, e) if e.get("tag").is_some() => {
                assert_eq!(tag as u64, e["tag"].as_u64().unwrap(), "tag mismatch on: {case}");
                let got: Vec<u8> = match bytes {
                    PrimitiveBytes::None => Vec::new(),
                    PrimitiveBytes::Bool(b) => vec![b],
                    PrimitiveBytes::Eight(a) => a.to_vec(),
                    PrimitiveBytes::Sixteen(a) => a.to_vec(),
                };
                assert_eq!(got, unhex(e["hex"].as_str().unwrap()), "payload mismatch on: {case}");
            }
            _ => panic!("decode outcome kind mismatch on: {case}"),
        }
    }

    #[test]
    fn test_cases() {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!("cases/abi.json")).expect("invalid JSON");

        // Table and stash live across cases, corpus order is the op sequence.
        let mut table = HandleTable::new();
        let mut stash = ErrorStash::new();
        let error_pair = |v: &serde_json::Value| -> Option<(u32, String)> {
            v.as_array().map(|a| (a[0].as_u64().unwrap() as u32, a[1].as_str().unwrap().to_string()))
        };

        for case in &cases {
            match case["kind"].as_str().unwrap() {
                "encode" => check_encode(case),
                "decode" => check_decode(case),
                "handle_put" => {
                    let h = table.put(bits(case["bits"].as_str().unwrap()));
                    assert_eq!(h as u64, case["expect"].as_u64().unwrap(), "put handle mismatch on: {case}");
                }
                "handle_get" => {
                    let got = table.get(case["handle"].as_u64().unwrap() as u32);
                    assert_eq!(got, case["expect"].as_str().map(bits), "get mismatch on: {case}");
                }
                "handle_release" => table.release(case["handle"].as_u64().unwrap() as u32),
                "stash_set" => {
                    let (kind, msg) = error_pair(&case["error"]).unwrap();
                    stash.set(kind, msg);
                }
                "stash_peek" => {
                    let got = stash.peek().map(|(k, m)| (k, m.to_string()));
                    assert_eq!(got, error_pair(&case["expect"]), "peek mismatch on: {case}");
                }
                "stash_take" => assert_eq!(stash.take(), error_pair(&case["expect"]), "take mismatch on: {case}"),
                "wire_roundtrip" => {
                    let value = wire_value(&case["value"]);
                    let mut buf = Vec::new();
                    value.encode_body(&mut buf);
                    let back = WireValue::decode_body(value.tag(), &buf);
                    assert_eq!(back.as_ref(), Some(&value), "wire roundtrip failed on: {case}");
                }
                "wire_malformed" => {
                    let tag = case["tag"].as_u64().unwrap() as u32;
                    let payload = unhex(case["hex"].as_str().unwrap());
                    assert_eq!(WireValue::decode_body(tag, &payload), None, "malformed wire accepted: {case}");
                }
                other => panic!("unknown case kind: {other}"),
            }
        }
    }

    /* The collector roots exactly the values a handle still holds, a released slot drops out until reused. */
    #[test]
    fn live_lists_held_values() {
        let mut table = HandleTable::new();
        let (a, b, _c) = (table.put(10), table.put(20), table.put(30));
        table.release(b);
        table.release(b);
        assert_eq!(table.live().collect::<Vec<_>>(), vec![10, 30]);
        table.release(a);
        assert_eq!(table.live().collect::<Vec<_>>(), vec![30]);
        table.put(40);
        assert_eq!(table.live().collect::<Vec<_>>(), vec![40, 30]);
        table.clear();
        assert_eq!(table.live().count(), 0);
    }

    /* Depth cases are generative, so they stay in Rust. */
    #[test]
    fn wire_depth_bomb_rejected() {
        let mut v = WireValue::List(vec![]);
        for _ in 0..(compiler::abi::MAX_WIRE_DEPTH + 2) { v = WireValue::List(vec![v]); }
        let mut buf = Vec::new();
        v.encode_body(&mut buf);
        assert_eq!(WireValue::decode_body(v.tag(), &buf), None);
    }

    #[test]
    fn wire_max_depth_accepted() {
        let mut v = WireValue::Int(1);
        for _ in 0..compiler::abi::MAX_WIRE_DEPTH { v = WireValue::List(vec![v]); }
        let mut buf = Vec::new();
        v.encode_body(&mut buf);
        assert_eq!(WireValue::decode_body(v.tag(), &buf).as_ref(), Some(&v));
    }
}
