//! #838: typed three-way intra-body merge on the merge path.
//!
//! Two branches that edit *disjoint* subtrees of the same function
//! (different match arms) auto-merge into one type-checked stage
//! instead of surfacing a whole-function ModifyModify conflict — the
//! genuinely-better-than-git case. Overlapping edits, and merges that
//! compose syntactically but not by type, still conflict.

use lex_store::{Store, DEFAULT_BRANCH};

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    (Store::open(tmp.path()).unwrap(), tmp)
}

/// Publish `src` as the whole program on `branch` (diff-based, like
/// `/v1/publish`), activating. Mirrors the HTTP publish path.
fn publish_program(s: &Store, branch: &str, src: &str) {
    s.set_current_branch(branch).unwrap();
    let prog = lex_syntax::parse_source(src).unwrap();
    let mut stages = lex_ast::canonicalize_program(&prog);
    lex_types::check_and_rewrite_program(&mut stages).expect("must type-check");
    // Build the old/new fn maps compute_diff wants.
    let old_fns = fn_map(s, branch);
    let new_fns: std::collections::BTreeMap<String, lex_ast::FnDecl> = stages.iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        }).collect();
    let report = lex_vcs::compute_diff(&old_fns, &new_fns, false);
    // These fixtures have no imports.
    let imports: lex_vcs::ImportMap = std::collections::BTreeMap::new();
    s.publish_program(branch, &stages, &report, &imports, true).expect("publish_program");
}

/// The current fn map on a branch (name -> FnDecl), for compute_diff.
fn fn_map(s: &Store, branch: &str) -> std::collections::BTreeMap<String, lex_ast::FnDecl> {
    let head = s.branch_head(branch).unwrap_or_default();
    let mut out = std::collections::BTreeMap::new();
    for (_sig, stage_id) in head {
        if let Ok(lex_ast::Stage::FnDecl(fd)) = s.get_ast(&stage_id) {
            out.insert(fd.name.clone(), fd);
        }
    }
    out
}

const BASE: &str = "\
fn classify(n :: Int) -> Int {
  match n {
    0 => 10,
    _ => 20,
  }
}
";

#[test]
fn disjoint_match_arm_edits_auto_merge_on_the_merge_path() {
    let (s, _tmp) = fresh();
    publish_program(&s, DEFAULT_BRANCH, BASE);
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();

    // feature (src) edits the second arm; main (dst) edits the first.
    publish_program(&s, "feature", "\
fn classify(n :: Int) -> Int {
  match n {
    0 => 10,
    _ => 22,
  }
}
");
    publish_program(&s, DEFAULT_BRANCH, "\
fn classify(n :: Int) -> Int {
  match n {
    0 => 11,
    _ => 20,
  }
}
");

    let report = s.merge("feature", DEFAULT_BRANCH).unwrap();
    assert!(report.conflicts.is_empty(),
        "disjoint arm edits must not conflict, got {:?}", report.conflicts);
    let entry = report.merged.iter().find(|m| m.from == "semantic")
        .expect("expected a semantic auto-merge entry");

    // Commit and confirm the merged body carries *both* edits.
    s.commit_merge(DEFAULT_BRANCH, &report).expect("merged head must type-check + land");
    let merged = match s.get_ast(&entry.stage_id).unwrap() {
        lex_ast::Stage::FnDecl(fd) => fd,
        _ => panic!("not a fn"),
    };
    let printed = lex_ast::print_stages(&[lex_ast::Stage::FnDecl(merged)]);
    assert!(printed.contains("11"), "ours' arm edit missing: {printed}");
    assert!(printed.contains("22"), "theirs' arm edit missing: {printed}");
}

#[test]
fn same_arm_edited_both_sides_still_conflicts() {
    let (s, _tmp) = fresh();
    publish_program(&s, DEFAULT_BRANCH, BASE);
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();

    // Both edit the *first* arm, differently.
    publish_program(&s, "feature", "\
fn classify(n :: Int) -> Int {
  match n {
    0 => 12,
    _ => 20,
  }
}
");
    publish_program(&s, DEFAULT_BRANCH, "\
fn classify(n :: Int) -> Int {
  match n {
    0 => 11,
    _ => 20,
  }
}
");

    let report = s.merge("feature", DEFAULT_BRANCH).unwrap();
    assert_eq!(report.conflicts.len(), 1, "overlapping arm edits must conflict");
    assert_eq!(report.conflicts[0].kind, "modify-modify");
    assert!(report.merged.iter().all(|m| m.from != "semantic"));
}
