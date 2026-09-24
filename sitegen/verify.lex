# Renders the docs site's "✓ verified" badge and embedded terminal
# recordings on each guide page (#1042). Both inputs here are GENERATED
# at CI build time, same footing as site/api-docs.json (#564/#567) — never
# hand-maintained prose, so a badge can never silently drift from what the
# toolchain actually did:
#
#   - site/verify-results.json   (scripts/verify-docs-examples.py) — one
#     entry per examples/*.lex file: did it `lex check --strict`, and, for
#     the subset whose doc comment shows a real `Run:` command, did that
#     command actually behave as documented when EXECUTED for real.
#   - site/recordings/manifest.json (scripts/record-docs-examples.py) —
#     file -> asciicast filename, for the same runnable subset, when a
#     real terminal recording of that Run: command was produced. This one
#     is soft-fail/best-effort (see that script's own doc comment), so a
#     recording can be legitimately absent even for a file that verified
#     clean — that's rendered honestly (a plain link to the .cast, or
#     nothing) rather than papered over.
#
# The on-page player is asciinema-player, vendored (self-hosted, not
# loaded from asciinema.org) by the workflow into site/vendor/ at build
# time — see .github/workflows/docs.yml. If that vendoring step didn't
# produce usable files (e.g. no network egress in a given CI environment),
# `player_ready` is false and pages fall back to a plain download link
# for the raw .cast instead of silently pointing at a third-party CDN.

import "std.fs" as fs
import "std.json" as json
import "std.str" as str
import "std.list" as list
import "std.tuple" as tuple
import "./jsonx" as jx

fn must_read(path :: Str) -> [fs_read] Str {
  match fs.read_to_string(path) { Ok(s) => s, Err(_) => "" }
}

# ── site/verify-results.json — a top-level JSON array ────────────────────

fn load_verify_results() -> [fs_read] List[Json] {
  match json.decode(must_read("site/verify-results.json")) {
    Ok(JList(xs)) => xs,
    _ => [],
  }
}

fn field_bool(obj :: List[Tuple[Str, Json]], key :: Str) -> Bool {
  match jx.obj_field(obj, key) { Some(JBool(b)) => b, _ => false }
}

# `ran` is `true` / `false` / `null` in the JSON — a real three-state, not
# a Bool with a fake default. `None` here covers both "explicitly null"
# and "key absent", which are the same thing for our purposes ("not
# applicable" — no documented Run: command to exercise).
fn field_bool_opt(obj :: List[Tuple[Str, Json]], key :: Str) -> Option[Bool] {
  match jx.obj_field(obj, key) { Some(JBool(b)) => Some(b), _ => None }
}

fn find_verify_result(results :: List[Json], file :: Str) -> Option[List[Tuple[Str, Json]]] {
  list.fold(results, None, fn (acc :: Option[List[Tuple[Str, Json]]], r :: Json) -> Option[List[Tuple[Str, Json]]] {
    match acc {
      Some(_) => acc,
      None => {
        let obj := jx.as_obj(r)
        match jx.field_str(obj, "file") == file {
          true  => Some(obj),
          false => None,
        }
      },
    }
  })
}

# ✓ / ✗ badge line for one example, from its verify-results.json entry.
# `""` when the file has no entry at all (shouldn't happen for anything
# `lex docs` found, but a page should never crash rendering over it).
fn badge_html(results :: List[Json], file :: Str) -> Str {
  match find_verify_result(results, file) {
    None => "",
    Some(r) => {
      let type_checked := field_bool(r, "type_checked")
      let lex_version  := jx.field_str(r, "lex_version")
      let verified_at  := jx.field_str(r, "verified_at")
      let suffix := " &middot; lex " + lex_version + " &middot; verified " + verified_at
      match type_checked {
        false =>
          "<p class=\"meta-line\"><span class=\"rejected\">&#10007; does not type-check</span>" + suffix + "</p>\n",
        true => match field_bool_opt(r, "ran") {
          Some(true) =>
            "<p class=\"meta-line\"><span class=\"ran\">&#10003; type-checks and runs clean</span>" + suffix + "</p>\n",
          Some(false) =>
            "<p class=\"meta-line\"><span class=\"rejected\">&#10007; type-checks, but its documented " +
            "<code>Run:</code> command did not behave as documented</span>" + suffix + "</p>\n",
          None =>
            "<p class=\"meta-line\"><span class=\"ran\">&#10003; type-checks</span>" + suffix + "</p>\n",
        },
      }
    },
  }
}

# ── site/recordings/manifest.json — file -> cast filename | null ─────────

fn load_recordings_manifest() -> [fs_read] List[Tuple[Str, Json]] {
  match json.decode(must_read("site/recordings/manifest.json")) {
    Ok(JObj(kvs)) => kvs,
    _ => [],
  }
}

fn find_cast(manifest :: List[Tuple[Str, Json]], file :: Str) -> Option[Str] {
  match jx.obj_field(manifest, file) {
    Some(JStr(name)) => Some(name),
    _ => None,
  }
}

# Both vendored asciinema-player files present and non-empty. Read once
# by the caller (build_guides in generate.lex) and threaded through, so
# every page doesn't re-read the same two files.
fn player_ready() -> [fs_read] Bool {
  not str.is_empty(must_read("site/vendor/asciinema-player.min.js")) and
  not str.is_empty(must_read("site/vendor/asciinema-player.min.css"))
}

# Embedded player (self-hosted — no runtime request to asciinema.org) or,
# when vendoring didn't come through in this build, a plain download link
# for the raw asciicast. `""` when this example has no recording at all —
# never a blank/empty player forced onto a page with nothing to show.
fn recording_html(manifest :: List[Tuple[Str, Json]], file :: Str, slug :: Str, ready :: Bool) -> Str {
  match find_cast(manifest, file) {
    None => "",
    Some(cast_name) => match ready {
      false =>
        "<p class=\"meta-line\">Terminal recording: <a href=\"../recordings/" + cast_name +
        "\">" + cast_name + "</a> (raw asciicast — no on-page player in this build; " +
        "see the docs.yml PR notes on asciinema-player vendoring).</p>\n",
      true =>
        "<link rel=\"stylesheet\" href=\"../vendor/asciinema-player.min.css\">\n" +
        "<div id=\"cast-" + slug + "\" class=\"cast-player\"></div>\n" +
        "<script src=\"../vendor/asciinema-player.min.js\"></script>\n" +
        "<script>AsciinemaPlayer.create('../recordings/" + cast_name + "', document.getElementById('cast-" +
        slug + "'), { autoPlay: false, preload: true, theme: 'monokai' });</script>\n",
    },
  }
}
