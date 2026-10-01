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

/// The hex digits of a label spelled `[<64 hex digits>]`, either case. The normalizer rejects `[`
/// and `]`, so no normalized label takes this form.
fn bracketed_label(label: &str) -> Option<&str> {
    label
        .strip_prefix('[')?
        .strip_suffix(']')
        .filter(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// A name with at least one bracketed labelhash: the other labels are normalized one by one and
/// the node is hashed from the given labelhashes.
fn normalize_bracketed_route_name(
    name: &str,
) -> Result<NormalizedRouteNameInput, RouteNameNormalizationError> {
    let mut labels = Vec::new();
    let mut labelhashes = Vec::new();
    let mut has_hashed_label = false;
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
            // Namespace inference reads `eth` and `base` as text, so they are spelled out.
            let known = ["eth", "base"]
                .into_iter()
                .find(|text| alloy_primitives::keccak256(text.as_bytes()).0 == labelhash);
            labels.push(known.map_or_else(|| label.to_owned(), str::to_owned));
            labelhashes.push(labelhash);
            has_hashed_label |= known.is_none();
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
        node: has_hashed_label.then(|| format!("0x{}", alloy_primitives::hex::encode(node))),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn bracketed(label: &str) -> String {
        format!(
            "[{}]",
            alloy_primitives::hex::encode(alloy_primitives::keccak256(label.as_bytes()))
        )
    }

    #[test]
    fn a_bracketed_label_addresses_its_node() {
        let input = normalize_inferred_route_name(&format!("{}.Alpha.eth", bracketed("x")))
            .expect("bracketed name parses");
        assert_eq!(input.namespace, bigname_storage::ENS_NAMESPACE);
        assert_eq!(
            input.normalized_name,
            format!("{}.alpha.eth", bracketed("x"))
        );
        assert!(input.corrected_input_normalization);
        assert_eq!(
            input.logical_name_id("ens"),
            bigname_storage::logical_name_id_for_name("ens", "x.alpha.eth")
        );
    }

    #[test]
    fn a_bracketed_suffix_label_is_read_as_its_text() {
        let input = normalize_inferred_route_name(&format!(
            "alice.{}.{}",
            bracketed("base"),
            bracketed("eth")
        ))
        .expect("bracketed name parses");
        assert_eq!(input.namespace, BASENAMES_NAMESPACE);
        assert_eq!(input.normalized_name, "alice.base.eth");
        assert_eq!(input.node, None);

        let input =
            normalize_inferred_route_name(&format!("{}.{}.eth", bracketed("x"), bracketed("base")))
                .expect("bracketed name parses");
        assert_eq!(input.namespace, BASENAMES_NAMESPACE);
        assert_eq!(
            input.logical_name_id("basenames"),
            bigname_storage::logical_name_id_for_name("basenames", "x.base.eth")
        );
    }
}
