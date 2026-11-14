//! The supervisor's pure parts (the run machine and where runs may work) stay
//! free of the runtime, I/O, and the shell around them. The crate depends on
//! Tokio for its task, so this is checked on the sources.

#[test]
fn pure_modules_stay_free_of_runtime_and_io_dependencies() {
    let sources = [
        ("run.rs", include_str!("run.rs")),
        ("workdir.rs", include_str!("workdir.rs")),
        ("project.rs", include_str!("project.rs")),
        ("entries.rs", include_str!("entries.rs")),
        ("files.rs", include_str!("files.rs")),
        ("session.rs", include_str!("session.rs")),
        ("sign_in.rs", include_str!("sign_in.rs")),
    ];
    let forbidden = [
        "tokio",
        "tracing",
        "futures_util",
        "std::fs",
        "std::net",
        "std::env",
        "std::process",
        "std::thread",
        "crate::host",
        "crate::task",
    ];
    for (file, source) in sources {
        for line in source.lines().map(str::trim_start) {
            if line.starts_with("//") {
                continue;
            }
            for dependency in forbidden {
                assert!(
                    !line.contains(&format!("{dependency}::"))
                        && !line.contains(&format!("use {dependency}")),
                    "{file} depends on {dependency}: {line}"
                );
            }
        }
    }
}
