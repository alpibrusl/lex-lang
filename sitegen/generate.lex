# The lex-lang docs site generator (#567). Reads:
#   - site/api-docs.json     — `lex --output json docs examples/` (#564)
#   - docs/*.md               — the curated reference set (see PR body for
#                                what was included/excluded and why)
#   - sitegen/landing.html    — the hand-authored landing content, adapted
#                                from the old docs/index.html
# and writes a static site to `_site/`. Dogfood: this is a Lex program,
# per lex-www's `generate.lex` convention — see the PR that added this
# file for why a Lex program was a good fit here (std.json + std.str +
# std.fs cover parsing the API-docs JSON and templating HTML) versus a
# fallback shell/Python script.
#
# Run from the repo root:
#   lex run --allow-effects fs_read,fs_write \
#     --allow-fs-read docs --allow-fs-read sitegen --allow-fs-read site \
#     --allow-fs-read examples --allow-fs-write _site \
#     sitegen/generate.lex main
#
# (`--allow-fs-read` is checked path-component-wise (Rust's
# `Path::starts_with`, crates/lex-runtime/src/handler/fs.rs), so
# `--allow-fs-read examples` does cover a nested read like
# `examples/agent_merge/v0_initial.lex` — but each *top-level* directory
# this program actually reads from — docs/, sitegen/, site/, and now
# examples/ (§ build_source_page, for the on-site source pages) — still
# needs its own flag; `--allow-fs-read .` does not cover
# `sitegen/landing.html` the way you'd expect.)

import "std.fs" as fs
import "std.json" as json
import "std.str" as str
import "std.list" as list
import "std.tuple" as tuple
import "./md" as md
import "./layout" as layout
import "./jsonx" as jx
import "./highlight" as hl

# ── Small IO helpers ─────────────────────────────────────────────────────

fn must_read(path :: Str) -> [fs_read] Str {
  match fs.read_to_string(path) {
    Ok(s) => s,
    Err(e) => "",
  }
}

fn write_page(path :: Str, html :: Str) -> [fs_read, fs_write] Unit {
  match fs.write(path, html) {
    Ok(_) => (),
    Err(e) => (),
  }
}

fn ensure_dirs() -> [fs_read, fs_write] Unit {
  match fs.mkdir_p("_site") { Ok(_) => (), Err(_) => () }
  match fs.mkdir_p("_site/getting-started") { Ok(_) => (), Err(_) => () }
  match fs.mkdir_p("_site/guides") { Ok(_) => (), Err(_) => () }
  match fs.mkdir_p("_site/vcs-hub") { Ok(_) => (), Err(_) => () }
  match fs.mkdir_p("_site/reference") { Ok(_) => (), Err(_) => () }
}

# ── Landing page ─────────────────────────────────────────────────────────

fn build_landing() -> [fs_read, fs_write] Unit {
  let body := must_read("sitegen/landing.html")
  let html := layout.page("", "Lex", "Lex is an effect-typed language: what code is allowed to do is checked before it runs and re-checked at runtime.", "home", false, body)
  write_page("_site/index.html", html)
}

# ── Getting Started (from docs/QUICKSTART.md) ────────────────────────────

fn fix_quickstart_links(src :: Str) -> Str {
  let s1 := str.replace(src, "](../README.md)", "](https://github.com/alpibrusl/lex-lang/blob/main/README.md)")
  let s2 := str.replace(s1, "](AGENT.md)", "](https://github.com/alpibrusl/lex-lang/blob/main/docs/AGENT.md)")
  s2
}

fn build_getting_started() -> [fs_read, fs_write] Unit {
  let src := fix_quickstart_links(must_read("docs/QUICKSTART.md"))
  let body := "<p class=\"meta-line\">From <a href=\"https://github.com/alpibrusl/lex-lang/blob/main/docs/QUICKSTART.md\">docs/QUICKSTART.md</a></p>\n" + md.to_html(src)
  let html := layout.page("../", "Getting Started", "Bootstrap a new Lex project from an empty directory to a green CI.", "start", false, body)
  write_page("_site/getting-started/index.html", html)
}

# ── VCS & Hub (from docs/VCS-HUB.md, verbatim content) ────────────────────

# A lightweight table of contents built straight from the source's own
# `##` headings, using the same slugify as the renderer so the anchors
# always agree with the ids `md.to_html` assigns.
fn vcs_hub_toc(src :: Str) -> Str {
  let lines := str.split(src, "\n")
  let items := list.filter(lines, fn (l :: Str) -> Bool {
    str.starts_with(l, "## ")
  })
  let lis := list.fold(items, "", fn (acc :: Str, l :: Str) -> Str {
    let text := str.trim(str.slice(l, 3, str.len(l)))
    acc + "<li><a href=\"#" + md.slugify(text) + "\">" + md.render_inline(text) + "</a></li>\n"
  })
  "<nav class=\"toc\"><h2>On this page</h2><ul>\n" + lis + "</ul></nav>\n"
}

fn build_vcs_hub() -> [fs_read, fs_write] Unit {
  let src := must_read("docs/VCS-HUB.md")
  let body := vcs_hub_toc(src) + md.to_html(src)
  let html := layout.page("../", "VCS & Hub", "lex-vcs's typed op-log and the hosted hub: install, publish, push, release, issues, the console.", "vcs", false, body)
  write_page("_site/vcs-hub/index.html", html)
}

# ── Reference (the other curated docs/*.md files) ─────────────────────────

type RefDoc = { path :: Str, slug :: Str, title :: Str, blurb :: Str }

fn ref_docs() -> List[RefDoc] {
  [
    { path: "docs/ROADMAP.md", slug: "roadmap", title: "Roadmap",
      blurb: "Cross-repo sequencing for the whole Lex project: the substrate, the runtime, the spec layer, the applications." },
    { path: "docs/STATUS.md", slug: "status", title: "Status",
      blurb: "The full production-ready / deferred capability table for lex-lang, current release." },
    { path: "docs/INVARIANTS.md", slug: "invariants", title: "Invariants",
      blurb: "An index of the contracts that back replay portability, content-addressed identity, and bytecode/trace stability." },
    { path: "docs/MIGRATING-0.10.md", slug: "migrating-0-10", title: "Migrating to 0.10",
      blurb: "The three breaking changes in the 0.10 stdlib-audit release, all caught at `lex check` time." },
    { path: "docs/cross-compile.md", slug: "cross-compile", title: "Cross-compiling",
      blurb: "Getting a `lex` binary running on a target you don't build natively for: release artifacts or `cross`." },
    { path: "docs/deploy.md", slug: "deploy", title: "Self-hosting the VCS server",
      blurb: "Running `lex serve` on a single VPS with Docker Compose + Caddy: one store, one user list, one set of branches." },
    { path: "docs/effect-row-polymorphism.md", slug: "effect-row-polymorphism", title: "Effect-row polymorphism",
      blurb: "The `[base | E]` open-row tail added in 0.10: what it is, when to reach for it, how to migrate to it." },
  ]
}

fn fix_status_links(src :: Str) -> Str {
  let s1 := str.replace(src, "](../CHANGELOG.md)", "](https://github.com/alpibrusl/lex-lang/blob/main/CHANGELOG.md)")
  let s2 := str.replace(s1, "](../bench/REPORT.md)", "](https://github.com/alpibrusl/lex-lang/blob/main/bench/REPORT.md)")
  let s3 := str.replace(s2, "](INVARIANTS.md)", "](invariants.html)")
  s3
}

fn fix_invariants_links(src :: Str) -> Str {
  let s1 := str.replace(src, "](../crates/lex-ast/tests/canonical.rs)", "](https://github.com/alpibrusl/lex-lang/blob/main/crates/lex-ast/tests/canonical.rs)")
  let s2 := str.replace(s1, "](../crates/lex-vcs/src/canonical.rs)", "](https://github.com/alpibrusl/lex-lang/blob/main/crates/lex-vcs/src/canonical.rs)")
  let s3 := str.replace(s2, "](AGENT_GUIDELINES.md)", "](https://github.com/alpibrusl/lex-lang/blob/main/docs/AGENT_GUIDELINES.md)")
  let s4 := str.replace(s3, "](design/canonicalization.md)", "](https://github.com/alpibrusl/lex-lang/blob/main/docs/design/canonicalization.md)")
  s4
}

fn fix_migrating_links(src :: Str) -> Str {
  str.replace(src, "](effect-row-polymorphism.md)", "](effect-row-polymorphism.html)")
}

fn fix_deploy_links(src :: Str) -> Str {
  str.replace(src, "](./design/trace-vs-vcs.md)", "](https://github.com/alpibrusl/lex-lang/blob/main/docs/design/trace-vs-vcs.md)")
}

fn fix_links_for(slug :: Str, src :: Str) -> Str {
  match slug {
    "status" => fix_status_links(src),
    "invariants" => fix_invariants_links(src),
    "migrating-0-10" => fix_migrating_links(src),
    "deploy" => fix_deploy_links(src),
    _ => src,
  }
}

fn build_reference_page(d :: RefDoc) -> [fs_read, fs_write] Unit {
  let raw := must_read(d.path)
  let fixed := fix_links_for(d.slug, raw)
  let src_note := "<p class=\"meta-line\">From <a href=\"https://github.com/alpibrusl/lex-lang/blob/main/" + d.path + "\">" + d.path + "</a></p>\n"
  let html := layout.page("../", d.title, d.blurb, "reference", false, src_note + md.to_html(fixed))
  write_page("_site/reference/" + d.slug + ".html", html)
}

fn ref_card(d :: RefDoc) -> Str {
  "<a class=\"card\" href=\"" + d.slug + ".html\"><h3>" + d.title + "</h3><p>" + d.blurb + "</p></a>\n"
}

fn build_reference_index() -> [fs_read, fs_write] Unit {
  let cards := list.fold(ref_docs(), "", fn (acc :: Str, d :: RefDoc) -> Str { acc + ref_card(d) })
  let gap_note :=
    "<h2>Not included here</h2>\n" +
    "<p><code>docs/AGENT.md</code> and <code>docs/AGENT_GUIDELINES.md</code> are written for coding agents " +
    "working <em>in</em> this repo (AGENT_GUIDELINES.md says so explicitly: \"Humans should read README.md first\") " +
    "rather than for readers of a public docs site, so they're linked to their GitHub source instead of " +
    "republished here: <a href=\"https://github.com/alpibrusl/lex-lang/blob/main/docs/AGENT.md\">AGENT.md</a>, " +
    "<a href=\"https://github.com/alpibrusl/lex-lang/blob/main/docs/AGENT_GUIDELINES.md\">AGENT_GUIDELINES.md</a>. " +
    "The same goes for <code>docs/design/*.md</code> &mdash; internal design rationale and measurement writeups " +
    "(arena plumbing, JIT roadmap, escape analysis, dispatch overhead, &hellip;), browsable at " +
    "<a href=\"https://github.com/alpibrusl/lex-lang/tree/main/docs/design\">docs/design/</a>. " +
    "There is also no stdlib API reference: the standard library is Rust-native builtins, not <code>.lex</code> " +
    "source, so <code>lex docs</code> has nothing to extract it from today &mdash; only the annotated " +
    "<a href=\"../guides/index.html\">examples/</a> are auto-documented from code.</p>\n"
  let body := "<h1>Reference</h1>\n<p class=\"lede\">The rest of the curated project documentation.</p>\n<div class=\"card-grid\">\n" + cards + "</div>\n" + gap_note
  let html := layout.page("../", "Reference", "Roadmap, status, invariants, migration notes, effect-row polymorphism, cross-compiling, self-hosting.", "reference", false, body)
  write_page("_site/reference/index.html", html)
}

fn build_reference() -> [fs_read, fs_write] Unit {
  list.fold(ref_docs(), (), fn (acc :: Unit, d :: RefDoc) -> [fs_read, fs_write] Unit { build_reference_page(d) })
  build_reference_index()
}

# ── Guides (rendered from site/api-docs.json) ─────────────────────────────

fn module_slug(file :: Str) -> Str {
  let s1 := match str.strip_prefix(file, "examples/") { Some(r) => r, None => file }
  let s2 := match str.strip_suffix(s1, ".lex") { Some(r) => r, None => s1 }
  str.replace(s2, "/", "-")
}

fn effect_badges(effects :: List[Str]) -> Str {
  match list.is_empty(effects) {
    true => "<span class=\"badge pure\">pure</span>",
    false => list.fold(effects, "", fn (acc :: Str, e :: Str) -> Str {
      acc + "<span class=\"badge effect\">" + md.escape_html(e) + "</span>"
    }),
  }
}

fn examples_html(examples :: List[Str]) -> Str {
  match list.is_empty(examples) {
    true => "",
    false => {
      let items := list.fold(examples, "", fn (acc :: Str, ex :: Str) -> Str {
        acc + "<li><code>" + md.escape_html(ex) + "</code></li>\n"
      })
      "<p class=\"meta-line\">Examples:</p>\n<ul>\n" + items + "</ul>\n"
    },
  }
}

fn function_html(f :: Json) -> Str {
  let fo := jx.as_obj(f)
  let name := jx.field_str(fo, "name")
  let sig := jx.field_str(fo, "signature")
  let effects := jx.str_list(jx.field_list(fo, "effects"))
  let examples := jx.str_list(jx.field_list(fo, "examples"))
  let sig_id := jx.field_str(fo, "sig_id")
  let doc := jx.field_str(fo, "doc")
  let doc_html := match str.is_empty(str.trim(doc)) { true => "", false => md.to_html(doc) }
  let sig_id_line := match str.is_empty(sig_id) {
    true => "",
    false => "<p class=\"meta-line\">sig_id <code>" + str.slice(sig_id, 0, 12) + "&hellip;</code></p>\n",
  }
  "<div class=\"fn-block\" id=\"fn-" + md.slugify(name) + "\">\n" +
  "<h3>" + md.escape_html(name) + "</h3>\n" +
  "<code class=\"fn-sig\">" + md.escape_html(sig) + "</code>\n" +
  effect_badges(effects) + "\n" +
  sig_id_line +
  doc_html +
  examples_html(examples) +
  "</div>\n"
}

# A real, on-site rendering of `file`'s full source — not just a link
# out to GitHub — so a reader can see the actual code without leaving
# doc.lexlang.org. Reads the literal file from disk (needs
# `--allow-fs-read examples`; see the header comment), not the
# `site/api-docs.json` JSON that `build_module_page` otherwise works
# from: that JSON only carries per-function signatures/doc comments,
# never full bodies.
fn build_source_page(file :: Str, slug :: Str) -> [fs_read, fs_write] Unit {
  let raw := must_read(file)
  let gh_url := "https://github.com/alpibrusl/lex-lang/blob/main/" + file
  let nav_line :=
    "<p class=\"meta-line src-line\">" +
    "<a href=\"" + slug + ".html\">&larr; " + md.escape_html(file) + " guide</a>" +
    "<a class=\"secondary\" href=\"" + gh_url + "\">view on GitHub ↗</a>" +
    "</p>\n"
  let body :=
    "<h1>" + md.escape_html(file) + "</h1>\n" +
    nav_line +
    "<pre class=\"lex-src\"><code>" + hl.to_html(raw) + "</code></pre>\n"
  let html := layout.page("../", file + " (source)", "Full on-site source for " + file + ", from the alpibrusl/lex-lang repository.", "guides", true, body)
  write_page("_site/guides/" + slug + "-src.html", html)
}

fn build_module_page(m :: Json) -> [fs_read, fs_write] Unit {
  let mo := jx.as_obj(m)
  let file := jx.field_str(mo, "file")
  let doc := jx.field_str(mo, "doc")
  let fns := jx.field_list(mo, "functions")
  let slug := module_slug(file)
  build_source_page(file, slug)
  let doc_html := match str.is_empty(str.trim(doc)) {
    true => "<p class=\"meta-line\">No module-level doc comment in this source file.</p>\n",
    false => md.to_html(doc),
  }
  let fns_html := list.fold(fns, "", fn (acc :: Str, f :: Json) -> Str { acc + function_html(f) })
  let src_note :=
    "<p class=\"meta-line src-line\">Source: " +
    "<a href=\"" + slug + "-src.html\"><code>" + file + "</code></a>" +
    "<a class=\"secondary\" href=\"https://github.com/alpibrusl/lex-lang/blob/main/" + file + "\">view on GitHub ↗</a>" +
    " &middot; " + int_to_str(list.len(fns)) + " function(s)</p>\n"
  let body := "<h1>" + md.escape_html(file) + "</h1>\n" + src_note + doc_html + "<h2>Functions</h2>\n" + fns_html
  let html := layout.page("../", file, "Guide and API reference for " + file + ", generated from its doc comments and live signatures.", "guides", false, body)
  write_page("_site/guides/" + slug + ".html", html)
}

fn int_to_str(n :: Int) -> Str {
  match n {
    0 => "0", 1 => "1", 2 => "2", 3 => "3", 4 => "4", 5 => "5",
    6 => "6", 7 => "7", 8 => "8", 9 => "9",
    _ => digits(n),
  }
}

fn digits(n :: Int) -> Str {
  match n < 10 {
    true => int_to_str(n),
    false => digits(n / 10) + int_to_str(n - ((n / 10) * 10)),
  }
}

fn guide_card(m :: Json) -> Str {
  let mo := jx.as_obj(m)
  let file := jx.field_str(mo, "file")
  let doc := jx.field_str(mo, "doc")
  let fns := jx.field_list(mo, "functions")
  let slug := module_slug(file)
  let status := match str.is_empty(str.trim(doc)) {
    true => "<span class=\"badge\">reference only</span>",
    false => "<span class=\"badge pure\">documented</span>",
  }
  # A plain `<div>`, not an `<a>`, because it needs *two* separate links
  # (the guide/doc page and the new on-site source page) — an `<a>`
  # can't nest another `<a>` inside it. `.card`'s box styling (border,
  # background, padding) applies the same to a div as it did to the
  # anchor this replaced.
  "<div class=\"card guide-card\"><h3>" + md.escape_html(file) + "</h3><p>" + status + " &middot; " + int_to_str(list.len(fns)) + " fn</p>" +
  "<p class=\"card-links\"><a href=\"" + slug + ".html\">Guide</a> &middot; <a href=\"" + slug + "-src.html\">Source</a></p></div>\n"
}

fn build_guides_index(modules :: List[Json]) -> [fs_read, fs_write] Unit {
  let cards := list.fold(modules, "", fn (acc :: Str, m :: Json) -> Str { acc + guide_card(m) })
  let body :=
    "<h1>Guides</h1>\n" +
    "<p class=\"lede\">One page per documented example under <code>examples/</code>, generated from " +
    "<code>lex --output json docs examples/</code>: the file's own doc comment as prose, plus every " +
    "function's live, type-checked signature and effect row.</p>\n" +
    "<div class=\"card-grid\">\n" + cards + "</div>\n"
  let html := layout.page("../", "Guides", "Every documented example under examples/, rendered from its own doc comment and live signatures.", "guides", false, body)
  write_page("_site/guides/index.html", html)
}

fn build_guides() -> [fs_read, fs_write] Unit {
  let api_src := must_read("site/api-docs.json")
  match json.decode(api_src) {
    Ok(root) => {
      let data := match jx.obj_field(jx.as_obj(root), "data") { Some(v) => v, None => JNull }
      let modules := jx.field_list(jx.as_obj(data), "modules")
      list.fold(modules, (), fn (acc :: Unit, m :: Json) -> [fs_read, fs_write] Unit { build_module_page(m) })
      build_guides_index(modules)
    },
    Err(e) => (),
  }
}

# ── Raw JSON passthrough ───────────────────────────────────────────────────
# The machine-readable artifact stays published too — issue #567's second
# consumer (an eventual doc-gen agent) reads this, not the rendered HTML.

fn copy_api_docs() -> [fs_read, fs_write] Unit {
  let src := must_read("site/api-docs.json")
  write_page("_site/api-docs.json", src)
}

fn write_cname() -> [fs_read, fs_write] Unit {
  write_page("_site/CNAME", "doc.lexlang.org\n")
}

fn main() -> [fs_read, fs_write] Unit {
  ensure_dirs()
  build_landing()
  build_getting_started()
  build_vcs_hub()
  build_reference()
  build_guides()
  copy_api_docs()
  write_cname()
}
