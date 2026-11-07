//! Input errors must fail loudly: a short read would otherwise give a
//! confidently wrong top k.

use groupcut::wiki::{load, GroupBy};
use std::path::PathBuf;
use std::process::Command;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("groupcut-io-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn missing_gz_is_an_error() {
    assert!(load(&[tmp("does-not-exist.gz")], GroupBy::Page).is_err());
}

#[test]
fn truncated_gz_is_an_error_and_intact_gz_loads() {
    let plain = tmp("pv");
    let text: String = (0..50_000)
        .map(|i| format!("en Page_{i} {} 0\n", i % 97 + 1))
        .collect();
    std::fs::write(&plain, text).unwrap();
    assert!(Command::new("gzip")
        .arg("-kf")
        .arg(&plain)
        .status()
        .unwrap()
        .success());
    let gz = plain.with_extension("gz");
    std::fs::rename(plain.with_file_name("pv.gz"), &gz).unwrap();

    let (keys, vals) = load(&[gz.clone()], GroupBy::Page).unwrap();
    assert_eq!(keys.len(), 50_000);
    assert_eq!(
        vals.iter().sum::<i64>(),
        (0..50_000).map(|i| i % 97 + 1).sum::<i64>()
    );

    let bytes = std::fs::read(&gz).unwrap();
    let cut = tmp("cut.gz");
    std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
    let err = load(&[cut], GroupBy::Page).unwrap_err();
    assert!(err.to_string().contains("refusing to answer"), "{err}");
}
