//! API that other crates call by name. A private item fails to compile here.

/// The inbox reader's store rebuild (plan D1) prunes the old device through
/// this function, so it has to be public.
#[test]
fn prune_stale_devices_is_public() {
    let _prune = aqua_matrix_agent::prune_stale_devices;
}
