//! Every closed vocabulary's labels, pinned. Labels are written to storage and
//! read from the wire, so renaming a variant must never change one silently.
//! Each label must also round-trip through `Display`, `FromStr`, and serde.
#![allow(
    clippy::unwrap_used,
    reason = "integration tests are a shell: helpers outside #[test] fail loudly"
)]

use ren_protocol::label::Labeled;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fmt::{Debug, Display},
    str::FromStr,
};

/// The labels of `T`, in declaration order, after checking that every
/// representation agrees with them.
fn labels<T>() -> Vec<&'static str>
where
    T: Labeled + Debug + Display + FromStr + Serialize + DeserializeOwned,
    <T as FromStr>::Err: Debug,
{
    T::ALL
        .iter()
        .copied()
        .map(|variant| {
            let label = variant.label();
            assert_eq!(variant.to_string(), label, "{variant:?}");
            assert_eq!(label.parse::<T>().unwrap(), variant, "{variant:?}");
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, format!("\"{label}\""), "{variant:?}");
            assert_eq!(serde_json::from_str::<T>(&json).unwrap(), variant);
            label
        })
        .collect()
}

macro_rules! table {
    ($($vocabulary:path),+ $(,)?) => {{
        let mut table = String::new();
        $(
            table.push_str(stringify!($vocabulary));
            table.push_str(":");
            for label in labels::<$vocabulary>() {
                table.push(' ');
                table.push_str(label);
            }
            table.push('\n');
        )+
        table
    }};
}

#[test]
fn every_label_is_pinned() {
    use ren_depot::{job, package, provision, source};
    let table = table![
        package::Kind,
        provision::Fit,
        source::Platform,
        source::ArchiveFormat,
        job::Stage,
        job::HostFailure,
    ];
    let pinned = include_str!("fixtures/labels.txt");
    assert!(
        table == pinned,
        "labels changed; the current table is:\n{table}"
    );
}
