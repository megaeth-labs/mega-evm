//! Runs the whole corpus through both arms.

use mega_differential::{corpus_dir, load_corpus, registry::Registry, registry_path, run};

#[test]
fn test_corpus_matches_the_oracle() {
    let scenarios = load_corpus(&corpus_dir()).unwrap();
    let registry = Registry::from_json(&std::fs::read_to_string(registry_path()).unwrap()).unwrap();

    let report = run(&scenarios, &registry);

    println!("{}", report.summary());
    assert!(report.is_clean(), "{}\n{}", report.summary(), report.failures());
}
