use termset_cli::testkit::Harness;

#[test]
fn scrollbar_visible() {
    let ws = "groups:\n  - name: G\n    sessions:\n      - name: sh\n        dir: ~\n        command: bash\n";
    let mut h = Harness::new(ws);
    let mut lines = String::new();
    for i in 1..=200 {
        lines.push_str(&format!("line {i:03} the quick brown fox jumps over the lazy dog\r\n"));
    }
    h.select("sh").feed("sh", &lines);
    h.scroll(40); // scroll up into scrollback
    let p = h.screenshot("scrollbar-scrolled");
    eprintln!("OUT {}", p.display());
    h.scroll(-1000); // back to bottom
    let p2 = h.screenshot("scrollbar-bottom");
    eprintln!("OUT {}", p2.display());
}
