use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NormalizedRouteNameInput {
    pub(crate) namespace: &'static str,
    pub(crate) normalized_name: String,
    pub(crate) corrected_input_normalization: bool,
    /// The node, when a label is spelled as its bracketed labelhash: `normalized_name` then keeps
    /// that spelling and names no row by itself.
    pub(crate) node: Option<String>,
}

impl NormalizedRouteNameInput {
    pub(crate) fn logical_name_id(&self, namespace: &str) -> String {
        match &self.node {
            Some(node) => format!("{namespace}:{node}"),
            None => bigname_storage::logical_name_id_for_name(namespace, &self.normalized_name),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RouteNameNormalizationError {
    pub(crate) message: String,
}

fn infer_resolution_namespace(name: &str) -> &'static str {
    if name == "base.eth" {
        return bigname_storage::ENS_NAMESPACE;
    }

    if name
        .strip_suffix(".base.eth")
        .is_some_and(|prefix| !prefix.is_empty())
    {
        BASENAMES_NAMESPACE
    } else {
        bigname_storage::ENS_NAMESPACE
    }
}

pub(crate) fn normalize_inferred_route_name(
    name: &str,
) -> Result<NormalizedRouteNameInput, RouteNameNormalizationError> {
    if name.is_empty() {
        return Err(RouteNameNormalizationError {
            message: "name must not be empty".to_owned(),
        });
    }
    if name
        .split('.')
        .any(|label| bracketed_label(label).is_some())
    {
        return normalize_bracketed_route_name(name);
    }
    let normalized = bigname_domain::normalization::normalize_name(name).map_err(|error| {
        RouteNameNormalizationError {
            message: error.message().to_owned(),
        }
    })?;
    Ok(NormalizedRouteNameInput {
        namespace: infer_resolution_namespace(&normalized.normalized_name),
        corrected_input_normalization: name != normalized.normalized_name,
        normalized_name: normalized.normalized_name,
        node: None,
    })
}

/// The hex digits of a label spelled `[<64 hex digits>]`, either case. ENSIP-15 disallows `[`
/// and `]`, so no normalized label takes this form.
fn bracketed_label(label: &str) -> Option<&str> {
    label
        .strip_prefix('[')?
        .strip_suffix(']')
        .filter(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// A name with at least one bracketed labelhash: the other labels are normalized one by one and
/// the node is hashed from the given labelhashes, as ensjs encodes an unknown label.
fn normalize_bracketed_route_name(
    name: &str,
) -> Result<NormalizedRouteNameInput, RouteNameNormalizationError> {
    let mut labels = Vec::new();
    let mut labelhashes = Vec::new();
    for label in name.split('.') {
        if let Some(hex) = bracketed_label(label) {
            if hex.bytes().any(|byte| byte.is_ascii_uppercase()) {
                return Err(RouteNameNormalizationError {
                    message: format!("bracketed labelhash {label} must be lowercase hex"),
                });
            }
            let mut labelhash = [0u8; 32];
            alloy_primitives::hex::decode_to_slice(hex, &mut labelhash)
                .expect("64 hex digits decode to 32 bytes");
            labels.push(label.to_owned());
            labelhashes.push(labelhash);
            continue;
        }
        let normalized = bigname_domain::normalization::normalize_label_under_suffix(label, &[])
            .map_err(|error| RouteNameNormalizationError {
                message: error.message().to_owned(),
            })?;
        labelhashes.push(alloy_primitives::keccak256(normalized.normalized_name.as_bytes()).0);
        labels.push(normalized.normalized_name);
    }
    let node = labelhashes.iter().rev().fold([0u8; 32], |node, labelhash| {
        alloy_primitives::keccak256([node, *labelhash].concat()).0
    });
    let normalized_name = labels.join(".");
    Ok(NormalizedRouteNameInput {
        namespace: infer_resolution_namespace(&normalized_name),
        corrected_input_normalization: name != normalized_name,
        normalized_name,
        node: Some(format!("0x{}", alloy_primitives::hex::encode(node))),
    })
}

pub(crate) const PROFILE_FALLBACK_RECORD_KEYS: &[&str] = &[
    "addr:60",
    "avatar",
    "contenthash",
    "text:description",
    "text:url",
    "text:email",
];
