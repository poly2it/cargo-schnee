use crate::nix_encoding::{compress_hash, hex_lower, nix_base32_encode};
use anyhow::Result;
use sha2::{Digest, Sha256};

/// Escape a string for ATerm format and write it (with surrounding quotes) to `out`.
pub(super) fn aterm_string(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for byte in s.bytes() {
        match byte {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'"' => out.extend_from_slice(b"\\\""),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

/// Serialize a derivation JSON to ATerm format, producing byte-identical output
/// to what `nix derivation add` writes into `.drv` files.
///
/// All object keys are explicitly sorted to match Nix ATerm field ordering,
/// regardless of whether serde_json uses BTreeMap or IndexMap internally.
pub(super) fn serialize_derivation_aterm(json: &serde_json::Value) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(4096);
    out.extend_from_slice(b"Derive(");

    // outputs: [("out","","r:sha256","")]
    let outputs = json["outputs"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("derivation JSON missing 'outputs' object"))?;
    out.push(b'[');
    let mut first = true;
    let mut output_keys: Vec<&String> = outputs.keys().collect();
    output_keys.sort();
    for name in output_keys {
        let info = &outputs[name];
        if !first {
            out.push(b',');
        }
        first = false;
        out.push(b'(');
        aterm_string(&mut out, name);
        out.push(b',');
        aterm_string(&mut out, ""); // path (empty for floating CA)
        out.push(b',');
        let hash_algo = info["hashAlgo"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("derivation output '{}' missing 'hashAlgo'", name))?;
        let method = info.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let algo_str = if method == "nar" {
            format!("r:{}", hash_algo)
        } else {
            hash_algo.to_string()
        };
        aterm_string(&mut out, &algo_str);
        out.push(b',');
        aterm_string(&mut out, ""); // hash (empty for floating CA)
        out.push(b')');
    }
    out.push(b']');
    out.push(b',');

    // inputDrvs: [("/nix/store/x.drv",["out"]),..]
    let input_drvs = json["inputDrvs"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("derivation JSON missing 'inputDrvs' object"))?;
    out.push(b'[');
    first = true;
    let mut input_drv_keys: Vec<&String> = input_drvs.keys().collect();
    input_drv_keys.sort();
    for path in input_drv_keys {
        let info = &input_drvs[path];
        if !first {
            out.push(b',');
        }
        first = false;
        out.push(b'(');
        aterm_string(&mut out, path);
        out.push(b',');
        let drv_outputs = info["outputs"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("inputDrv '{}' missing 'outputs' array", path))?;
        out.push(b'[');
        let mut first_out = true;
        for o in drv_outputs {
            if !first_out {
                out.push(b',');
            }
            first_out = false;
            aterm_string(
                &mut out,
                o.as_str().ok_or_else(|| {
                    anyhow::anyhow!("non-string in inputDrv outputs for '{}'", path)
                })?,
            );
        }
        out.push(b']');
        out.push(b')');
    }
    out.push(b']');
    out.push(b',');

    // inputSrcs: ["/nix/store/...",...]
    let input_srcs = json["inputSrcs"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("derivation JSON missing 'inputSrcs' array"))?;
    out.push(b'[');
    first = true;
    for src in input_srcs {
        if !first {
            out.push(b',');
        }
        first = false;
        aterm_string(
            &mut out,
            src.as_str()
                .ok_or_else(|| anyhow::anyhow!("non-string in inputSrcs"))?,
        );
    }
    out.push(b']');
    out.push(b',');

    // system
    aterm_string(
        &mut out,
        json["system"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("derivation JSON missing 'system' string"))?,
    );
    out.push(b',');

    // builder
    aterm_string(
        &mut out,
        json["builder"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("derivation JSON missing 'builder' string"))?,
    );
    out.push(b',');

    // args
    let args = json["args"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("derivation JSON missing 'args' array"))?;
    out.push(b'[');
    first = true;
    for arg in args {
        if !first {
            out.push(b',');
        }
        first = false;
        aterm_string(
            &mut out,
            arg.as_str()
                .ok_or_else(|| anyhow::anyhow!("non-string in args array"))?,
        );
    }
    out.push(b']');
    out.push(b',');

    // env: [("key","value"),..]
    let env = json["env"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("derivation JSON missing 'env' object"))?;
    out.push(b'[');
    first = true;
    let mut env_keys: Vec<&String> = env.keys().collect();
    env_keys.sort();
    for key in env_keys {
        let val = &env[key];
        if !first {
            out.push(b',');
        }
        first = false;
        out.push(b'(');
        aterm_string(&mut out, key);
        out.push(b',');
        aterm_string(
            &mut out,
            val.as_str()
                .ok_or_else(|| anyhow::anyhow!("non-string env value for key '{}'", key))?,
        );
        out.push(b')');
    }
    out.push(b']');

    out.push(b')');
    Ok(out)
}

/// Compute the store path for a `.drv` file from its ATerm content and references.
///
/// This mirrors Nix's `computeStorePathForText`:
///   inner = sha256(aterm)
///   fingerprint = "text:<refs>:sha256:<hex(inner)>:/nix/store:<name>"
///   outer = sha256(fingerprint)
///   path = "/nix/store/" + nix_base32(compress(outer, 20)) + "-" + name
pub(super) fn compute_drv_store_path(name: &str, aterm: &[u8], refs: &[&str]) -> String {
    let inner_hash = Sha256::digest(aterm);

    let mut sorted_refs: Vec<&str> = refs.to_vec();
    sorted_refs.sort();

    let refs_joined = if sorted_refs.is_empty() {
        String::new()
    } else {
        format!(":{}", sorted_refs.join(":"))
    };
    let fingerprint = format!(
        "text{}:sha256:{}:/nix/store:{}",
        refs_joined,
        hex_lower(&inner_hash),
        name,
    );

    let outer_hash = Sha256::digest(fingerprint.as_bytes());
    let compressed = compress_hash(&outer_hash, 20);
    format!("/nix/store/{}-{}", nix_base32_encode(&compressed), name)
}

/// Collect references (inputSrcs + inputDrvs keys) from a derivation JSON.
pub(super) fn collect_drv_refs(json: &serde_json::Value) -> Vec<String> {
    let mut refs = Vec::new();
    if let Some(input_srcs) = json["inputSrcs"].as_array() {
        for src in input_srcs {
            if let Some(s) = src.as_str() {
                refs.push(s.to_string());
            }
        }
    }
    if let Some(input_drvs) = json["inputDrvs"].as_object() {
        for path in input_drvs.keys() {
            refs.push(path.clone());
        }
    }
    refs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nix_encoding::NIX_BASE32;
    use proptest::prelude::*;

    #[test]
    fn aterm_string_basic() {
        let mut out = Vec::new();
        aterm_string(&mut out, "hello");
        assert_eq!(out, b"\"hello\"");
    }

    #[test]
    fn aterm_string_escaping() {
        let mut out = Vec::new();
        aterm_string(&mut out, "a\\b\"c\nd\te");
        assert_eq!(out, b"\"a\\\\b\\\"c\\nd\\te\"");
    }

    #[test]
    fn aterm_string_empty() {
        let mut out = Vec::new();
        aterm_string(&mut out, "");
        assert_eq!(out, b"\"\"");
    }

    #[test]
    fn compute_drv_store_path_deterministic() {
        let aterm =
            b"Derive([],[],[],\"x86_64-linux\",\"/bin/sh\",[\"-c\",\"echo\"],[(\"out\",\"\")])";
        let refs: Vec<&str> = vec![];
        let p1 = compute_drv_store_path("test.drv", aterm, &refs);
        let p2 = compute_drv_store_path("test.drv", aterm, &refs);
        assert_eq!(p1, p2);
    }

    #[test]
    fn compute_drv_store_path_format() {
        let aterm = b"Derive(stuff)";
        let p = compute_drv_store_path("test.drv", aterm, &[]);
        assert!(p.starts_with("/nix/store/"));
        assert!(p.ends_with("-test.drv"));
        let hash = &p["/nix/store/".len()..p.len() - "-test.drv".len()];
        assert_eq!(hash.len(), 32);
        assert!(hash.bytes().all(|b| NIX_BASE32.contains(&b)));
    }

    #[test]
    fn compute_drv_store_path_ref_order_independent() {
        let aterm = b"Derive(test)";
        let p1 = compute_drv_store_path("x.drv", aterm, &["/nix/store/aaa-a", "/nix/store/bbb-b"]);
        let p2 = compute_drv_store_path("x.drv", aterm, &["/nix/store/bbb-b", "/nix/store/aaa-a"]);
        assert_eq!(p1, p2);
    }

    #[test]
    fn serialize_derivation_aterm_minimal() {
        let json = serde_json::json!({
            "name": "test",
            "system": "x86_64-linux",
            "builder": "/bin/sh",
            "args": ["-c", "echo hi"],
            "env": {"out": "/placeholder"},
            "inputDrvs": {},
            "inputSrcs": [],
            "outputs": {"out": {"hashAlgo": "sha256", "method": "nar"}}
        });
        let aterm = serialize_derivation_aterm(&json).unwrap();
        let s = String::from_utf8(aterm).unwrap();
        assert!(s.starts_with("Derive("));
        assert!(s.ends_with(')'));
        assert!(s.contains("\"x86_64-linux\""));
    }

    #[test]
    fn serialize_derivation_aterm_missing_outputs() {
        let json = serde_json::json!({
            "system": "x86_64-linux",
            "builder": "/bin/sh",
            "args": [],
            "env": {},
            "inputDrvs": {},
            "inputSrcs": []
        });
        let err = serialize_derivation_aterm(&json).unwrap_err();
        assert!(
            err.to_string().contains("outputs"),
            "expected 'outputs' in error: {}",
            err
        );
    }

    #[test]
    fn serialize_derivation_aterm_missing_env() {
        let json = serde_json::json!({
            "outputs": {"out": {"hashAlgo": "sha256", "method": "nar"}},
            "system": "x86_64-linux",
            "builder": "/bin/sh",
            "args": [],
            "inputDrvs": {},
            "inputSrcs": []
        });
        let err = serialize_derivation_aterm(&json).unwrap_err();
        assert!(
            err.to_string().contains("env"),
            "expected 'env' in error: {}",
            err
        );
    }

    #[test]
    fn serialize_derivation_aterm_non_string_env_value() {
        let json = serde_json::json!({
            "outputs": {"out": {"hashAlgo": "sha256", "method": "nar"}},
            "system": "x86_64-linux",
            "builder": "/bin/sh",
            "args": [],
            "env": {"out": 42},
            "inputDrvs": {},
            "inputSrcs": []
        });
        let err = serialize_derivation_aterm(&json).unwrap_err();
        assert!(
            err.to_string().contains("non-string env value"),
            "expected 'non-string env value' in error: {}",
            err
        );
    }

    #[test]
    fn serialize_derivation_aterm_non_string_arg() {
        let json = serde_json::json!({
            "outputs": {"out": {"hashAlgo": "sha256", "method": "nar"}},
            "system": "x86_64-linux",
            "builder": "/bin/sh",
            "args": [42],
            "env": {"out": "/placeholder"},
            "inputDrvs": {},
            "inputSrcs": []
        });
        let err = serialize_derivation_aterm(&json).unwrap_err();
        assert!(
            err.to_string().contains("non-string in args"),
            "expected 'non-string in args' in error: {}",
            err
        );
    }

    fn make_test_derivation(name: &str, arg: &str) -> serde_json::Value {
        serde_json::json!({
            "name": name, "system": "x86_64-linux", "builder": "/bin/sh",
            "args": ["-c", arg], "env": {"out": "/placeholder"},
            "inputDrvs": {}, "inputSrcs": [],
            "outputs": {"out": {"hashAlgo": "sha256", "method": "nar"}}
        })
    }

    proptest! {
        #[test]
        fn prop_aterm_format(
            name in "[a-z][a-z0-9-]{0,20}",
            arg in "[a-zA-Z0-9 _/.-]{0,100}",
        ) {
            let json = make_test_derivation(&name, &arg);
            let aterm = serialize_derivation_aterm(&json).unwrap();
            let s = String::from_utf8(aterm).unwrap();
            prop_assert!(s.starts_with("Derive("));
            prop_assert!(s.ends_with(')'));
        }

        #[test]
        fn prop_aterm_deterministic(
            name in "[a-z][a-z0-9-]{0,20}",
            arg in "[a-zA-Z0-9 _/.-]{0,100}",
        ) {
            let json = make_test_derivation(&name, &arg);
            let a1 = serialize_derivation_aterm(&json).unwrap();
            let a2 = serialize_derivation_aterm(&json).unwrap();
            prop_assert_eq!(a1, a2);
        }

        #[test]
        fn prop_drv_store_path_format(
            name in "[a-z][a-z0-9-]{0,20}",
            content in proptest::collection::vec(any::<u8>(), 1..200),
        ) {
            let drv_name = format!("{}.drv", name);
            let path = compute_drv_store_path(&drv_name, &content, &[]);
            prop_assert!(path.starts_with("/nix/store/"));
            let after_store = path.strip_prefix("/nix/store/").unwrap();
            let hash_part = &after_store[..32];
            prop_assert_eq!(hash_part.len(), 32);
            prop_assert!(hash_part.bytes().all(|b| crate::nix_encoding::NIX_BASE32.contains(&b)));
        }

        #[test]
        fn prop_drv_store_path_ref_order_independent(
            content in proptest::collection::vec(any::<u8>(), 1..100),
        ) {
            let refs1 = ["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a", "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b"];
            let refs2 = ["/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b", "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a"];
            let p1 = compute_drv_store_path("x.drv", &content, &refs1);
            let p2 = compute_drv_store_path("x.drv", &content, &refs2);
            prop_assert_eq!(p1, p2);
        }
    }
}
