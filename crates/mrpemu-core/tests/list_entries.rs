//! Temporary: lists package entries. Not a regression gate.
#[test]
#[ignore = "manual probe"]
fn list_entries() {
    let path = std::env::var("PROBE_PKG").unwrap();
    let data = std::fs::read(&path).unwrap();
    let package = mrpemu_core::package::MrpPackage::parse(data).unwrap();
    for entry in package.entries() {
        println!("{:24} len={}", entry.name, entry.size);
    }
}
