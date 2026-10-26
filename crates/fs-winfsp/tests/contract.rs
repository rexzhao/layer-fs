use fs_winfsp::MockBackend;

/// The mock's instantiation of every scenario. The real backend's lives in `fs-api/tests/real_backend_contract.rs`;
/// criterion 7a requires **both** to exist (adding a third is fine, removing one is not).
#[test]
fn the_mock_satisfies_the_trait_contract() {
    let backend = MockBackend::new();
    fs_winfsp::scenarios::scenario_identity_across_rename::<MockBackend>(&backend);
    let backend = MockBackend::new();
    fs_winfsp::scenarios::scenario_supersede_truncates_in_place::<MockBackend>(&backend);
    let backend = MockBackend::new();
    fs_winfsp::scenarios::scenario_removal_cases::<MockBackend>(&backend);
    let backend = MockBackend::new();
    fs_winfsp::scenarios::scenario_read_dir_by_id::<MockBackend>(&backend);
}
