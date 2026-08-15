//! The committed CRD against the types it was generated from.
//!
//! `operator/deploy/crd.yaml` is committed so installing the operator needs
//! nothing but `kubectl`. That convenience is also how a manifest goes stale:
//! a field added to `FastllmProxySpec` and not regenerated is a field the API
//! server rejects, at apply time, in someone else's cluster.
//!
//! Fix a failure by regenerating, never by editing the YAML:
//!
//!   cargo run -p fastllm-operator --bin crdgen > operator/deploy/crd.yaml

#[test]
fn the_committed_crd_matches_the_rust_types() {
    let committed =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/deploy/crd.yaml"))
            .expect("operator/deploy/crd.yaml");

    assert_eq!(
        committed.trim_end(),
        fastllm_operator::crd::manifest_yaml().trim_end(),
        "operator/deploy/crd.yaml is stale — regenerate it with \
         `cargo run -p fastllm-operator --bin crdgen > operator/deploy/crd.yaml`"
    );
}

/// The rules are the reason `manifest()` exists at all: a schema that
/// described the fields but let `encryptionKey` be edited would be a CRD that
/// accepts the one change nothing downstream can survive.
#[test]
fn the_manifest_carries_the_validations_the_schema_cannot_express() {
    let crd = fastllm_operator::crd::manifest();
    let key = crd
        .pointer("/spec/versions/0/schema/openAPIV3Schema/properties/spec/properties/encryptionKey/x-kubernetes-validations")
        .expect("encryptionKey immutability rule");
    assert_eq!(key[0]["rule"], "self == oldSelf");
    assert!(
        crd.pointer("/spec/versions/0/schema/openAPIV3Schema/properties/spec/properties/proxy/properties/autoscaling/x-kubernetes-validations")
            .is_some(),
        "autoscaling bounds rule"
    );
    // Exposing the admin plane in the clear would publish decrypted upstream
    // credentials; the API server refuses it rather than the controller
    // noticing afterwards.
    let control = crd
        .pointer("/spec/versions/0/schema/openAPIV3Schema/properties/spec/properties/control/x-kubernetes-validations")
        .expect("control exposure rule");
    assert!(
        control[0]["rule"]
            .as_str()
            .unwrap()
            .contains("tlsSecretName"),
        "{control}"
    );
}
