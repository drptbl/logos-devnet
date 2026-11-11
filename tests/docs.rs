//! What the docs tell people to install and use is this release.

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn read(path: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}

#[test]
fn the_readme_installs_and_uses_this_release() {
    let readme = read("README.md");
    assert!(
        readme.contains(&format!("v=v{VERSION} ")),
        "the install example"
    );
    assert!(
        readme.contains(&format!("uses: drptbl/logos-devnet@v{VERSION}")),
        "the CI example"
    );
}

#[test]
fn the_workflow_example_uses_this_release() {
    let workflow = read("examples/github-actions/devnet.yml");
    assert!(workflow.contains(&format!("uses: drptbl/logos-devnet@v{VERSION}")));
}
