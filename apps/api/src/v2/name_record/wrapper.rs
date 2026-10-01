use serde_json::Value;

use super::super::{
    V2Error, V2Result,
    vocab::{WrapperFuses, WrapperState},
};

const NON_PARENT_CONTROLLED_FUSES: u32 = 0x0000_FFFF;

pub(crate) fn wrapper_metadata(
    declared_summary: &Value,
) -> V2Result<Option<(WrapperState, WrapperFuses)>> {
    let state_value = declared_summary.get("wrapper_state");
    let fuses_value = declared_summary.get("wrapper_fuses");
    if state_value.is_none() && fuses_value.is_none() {
        return Ok(None);
    }
    if state_value.is_none() || fuses_value.is_none() {
        return Err(invalid_wrapper_metadata());
    }

    let state = state_value
        .and_then(Value::as_str)
        .and_then(WrapperState::from_wire)
        .ok_or_else(invalid_wrapper_metadata)?;
    let fuses =
        WrapperFuses::from_summary(declared_summary).ok_or_else(invalid_wrapper_metadata)?;
    if !wrapper_lifecycle_matches_fuses(state, fuses) {
        return Err(invalid_wrapper_metadata());
    }
    Ok(Some((state, fuses)))
}

/// Whether a NameWrapper lifecycle label agrees with a fuse word; shared by name detail and the
/// `restrictions` block so both reject an inconsistent projection identically.
pub(crate) const fn wrapper_lifecycle_matches_fuses(
    state: WrapperState,
    fuses: WrapperFuses,
) -> bool {
    let has_locked_pair = fuses.cannot_unwrap && fuses.parent_cannot_control;
    // Any non-parent-controlled fuse requires both PARENT_CANNOT_CONTROL and
    // CANNOT_UNWRAP, including unnamed low-word bits.
    // (upstream: .refs/ens_v1/contracts/wrapper/NameWrapper.sol:L1058-L1066 @ ens_v1@91c966f)
    // (upstream: .refs/ens_v1/contracts/wrapper/INameWrapper.sol:L22 @ ens_v1@91c966f)
    if fuses.fuses & NON_PARENT_CONTROLLED_FUSES != 0 && !has_locked_pair {
        return false;
    }
    // .eth second-level wrapping always burns PARENT_CANNOT_CONTROL with
    // IS_DOT_ETH, and IS_DOT_ETH is excluded from user-settable fuses.
    // (upstream: .refs/ens_v1/contracts/wrapper/NameWrapper.sol:L1013 @ ens_v1@91c966f)
    // (upstream: .refs/ens_v1/contracts/wrapper/INameWrapper.sol:L24 @ ens_v1@91c966f)
    if fuses.is_dot_eth && !fuses.parent_cannot_control {
        return false;
    }

    match state {
        WrapperState::Wrapped => !fuses.cannot_unwrap && !fuses.parent_cannot_control,
        WrapperState::Emancipated => !fuses.cannot_unwrap && fuses.parent_cannot_control,
        WrapperState::Locked => has_locked_pair,
    }
}

/// The served `manager` (docs/api-v1.md, Manager): the account that can change the name's
/// records, the owner of a name with no NameWrapper state and the token holder, the registrant, of
/// a wrapped name in any state.
pub(crate) fn served_manager(
    declared_summary: &Value,
    owner: Option<&String>,
    registrant: Option<&String>,
) -> Option<String> {
    if declared_summary.get("wrapper_state").is_some() {
        registrant.cloned()
    } else {
        owner.cloned()
    }
}

fn invalid_wrapper_metadata() -> V2Error {
    V2Error::internal_error("stored wrapper metadata is inconsistent")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::served_manager;

    #[test]
    fn manager_is_the_owner_unwrapped_and_the_holder_in_every_wrapper_state() {
        let owner = "0xowner".to_owned();
        let holder = "0xholder".to_owned();
        for (summary, expected) in [
            (json!({}), Some(&owner)),
            (json!({"wrapper_state": "wrapped"}), Some(&holder)),
            (json!({"wrapper_state": "emancipated"}), Some(&holder)),
            (json!({"wrapper_state": "locked"}), Some(&holder)),
        ] {
            assert_eq!(
                served_manager(&summary, Some(&owner), Some(&holder)).as_ref(),
                expected,
                "{summary}"
            );
        }
        assert_eq!(
            served_manager(&json!({"wrapper_state": "locked"}), Some(&owner), None),
            None
        );
    }
}
