//! `maybe_rtl_reverse` against `TextUtils.maybe_rtl_reverse` in Rails
//! (`tests/fixtures/rtl_golden.json`, from `rust/scripts/golden/rtl_text.rb`).

#[test]
fn reorders_like_rails() {
    let golden: Vec<(String, String)> =
        serde_json::from_str(include_str!("fixtures/rtl_golden.json")).unwrap();
    let failures: Vec<String> = golden
        .iter()
        .filter_map(|(input, expected)| {
            let actual = loco_domain::bidi::maybe_rtl_reverse(input).unwrap();
            (&actual != expected).then(|| format!("{input:?}: {actual:?} != {expected:?}"))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
