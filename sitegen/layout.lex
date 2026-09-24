# Shared page shell: one <style> block, one nav bar, one footer, reused
# by every generated page so the site reads as one thing rather than a
# pile of independently-styled fragments. Palette follows docs/index.html
# (light-default, `prefers-color-scheme: dark` override) and stays close
# to lex-www's accent hues so the two sites feel related without sharing
# markup.
#
# Theme: `:root` carries the light values as the unconditional default.
# The dark values (`dark_vars()`, one shared string so the two copies
# below can't drift) are layered in twice — under `@media
# (prefers-color-scheme: dark)` guarded by `:root:not([data-theme="light"])`
# so an explicit *light* override always wins over a dark OS signal, and
# again, unconditionally, under `:root[data-theme="dark"]` so an explicit
# *dark* override always wins even when the OS/browser is reporting
# light (a managed profile, a preview tool, or just daytime — see the PR
# this followed up on). No `data-theme` attribute (the default, and what
# "auto" means) falls through to the media query alone. See
# `theme_head_script()` / `theme_toggle_script()` for how `data-theme`
# gets set.

import "std.str" as str

fn dark_vars() -> Str {
  "
      --fg: #e8eaed; --muted: #a1a8b8; --bg: #15171c; --card: #1c1f26;
      --border: #2d313b; --accent: #6f9aff; --rule: #2d313b;
      --code-bg: #11141a; --good: #4dba6a; --bad: #ff6b65;
      --tok-kw: #c678dd; --tok-ty: #61afef; --tok-st: #98c379;
      --tok-cm: #7f8794; --tok-nu: #d19a66; --tok-ef: #ff6b65;
  "
}

fn base_css() -> Str {
  "
  :root {
    --fg: #1d1f24; --muted: #5a6072; --bg: #fcfcfd; --card: #ffffff;
    --border: #e3e6ec; --accent: #2b5cd9; --rule: #d8dde6;
    --code-bg: #f5f7fa; --good: #1a7f37; --bad: #b42318;
    --tok-kw: #a626a4; --tok-ty: #0184bc; --tok-st: #50a14f;
    --tok-cm: #8a9099; --tok-nu: #c18401; --tok-ef: #b42318;
  }
  @media (prefers-color-scheme: dark) {
    :root:not([data-theme=\"light\"]) {" + dark_vars() + "}
  }
  :root[data-theme=\"dark\"] {" + dark_vars() + "}
  * { box-sizing: border-box; }
  body {
    margin: 0; background: var(--bg); color: var(--fg);
    font: 16px/1.6 -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif;
    -webkit-font-smoothing: antialiased;
  }
  .wrap { max-width: 900px; margin: 0 auto; padding: 0 24px; }
  a { color: var(--accent); }
  nav.topnav { border-bottom: 1px solid var(--rule); position: sticky; top: 0; background: var(--bg); z-index: 10; }
  nav.topnav .wrap { display: flex; align-items: center; gap: 4px; flex-wrap: wrap; padding-top: 14px; padding-bottom: 14px; max-width: 1080px; }
  nav.topnav .brand { font-weight: 700; margin-right: 18px; text-decoration: none; color: var(--fg); font-size: 15px; letter-spacing: -0.01em; }
  nav.topnav a.navlink { text-decoration: none; color: var(--muted); font-size: 14px; padding: 6px 10px; border-radius: 6px; }
  nav.topnav a.navlink:hover { color: var(--fg); background: var(--card); }
  nav.topnav a.navlink.active { color: var(--fg); font-weight: 600; background: var(--card); border: 1px solid var(--border); }
  nav.topnav a.gh { margin-left: auto; }
  main.wrap { max-width: 860px; padding-top: 40px; padding-bottom: 60px; }
  main.wrap.wide { max-width: 1080px; }
  h1 { font-size: 34px; line-height: 1.2; margin: 0 0 16px; letter-spacing: -0.02em; }
  h2 { font-size: 24px; margin: 40px 0 14px; letter-spacing: -0.01em; scroll-margin-top: 70px; }
  h3 { font-size: 17px; margin: 26px 0 8px; scroll-margin-top: 70px; }
  h1[id], h2[id], h3[id] { scroll-margin-top: 70px; }
  p, li { margin: 0 0 12px; }
  p.lede { font-size: 17px; color: var(--muted); }
  pre, code { font-family: ui-monospace, SF Mono, Menlo, Consolas, monospace; }
  pre {
    background: var(--code-bg); border: 1px solid var(--border); border-radius: 8px;
    padding: 14px 16px; overflow-x: auto; font-size: 13.5px; line-height: 1.55;
  }
  code { background: var(--code-bg); padding: 1px 5px; border-radius: 3px; font-size: 0.92em; }
  pre code { background: none; padding: 0; }
  blockquote { margin: 0 0 16px; padding: 4px 16px; border-left: 3px solid var(--accent); color: var(--muted); }
  blockquote p { margin: 0; }
  table { width: 100%; border-collapse: collapse; margin: 8px 0 20px; font-size: 14.5px; }
  th, td { text-align: left; padding: 9px 12px; border-bottom: 1px solid var(--rule); vertical-align: top; }
  th { font-weight: 600; color: var(--muted); font-size: 12.5px; text-transform: uppercase; letter-spacing: 0.04em; }
  hr { border: none; border-top: 1px solid var(--rule); margin: 32px 0; }
  ul, ol { padding-left: 22px; }
  .badge { display: inline-block; font-size: 11.5px; font-weight: 600; padding: 1px 8px; border-radius: 20px; margin: 0 4px 4px 0; background: var(--code-bg); border: 1px solid var(--border); color: var(--muted); }
  .badge.effect { color: var(--bad); border-color: var(--bad); }
  .badge.pure { color: var(--good); border-color: var(--good); }
  .card-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(230px, 1fr)); gap: 14px; margin: 18px 0 28px; }
  .card { display: block; text-decoration: none; background: var(--card); border: 1px solid var(--border); border-radius: 10px; padding: 16px 18px; color: var(--fg); }
  .card:hover { border-color: var(--accent); }
  .card h3 { margin: 0 0 6px; font-size: 15px; }
  .card p { margin: 0; color: var(--muted); font-size: 13px; }
  .card-links { margin-top: 8px !important; }
  .card-links a { font-weight: 600; margin-right: 4px; }
  .fn-block { border: 1px solid var(--border); border-radius: 10px; padding: 16px 18px; margin: 0 0 18px; background: var(--card); }
  .fn-block h3 { margin: 0 0 8px; font-size: 15.5px; }
  .fn-sig { display: block; margin: 6px 0 10px; white-space: pre-wrap; word-break: break-word; }
  .side-index { columns: 2; column-gap: 24px; margin: 0 0 24px; padding-left: 20px; font-size: 14px; }
  .side-index li { break-inside: avoid; margin-bottom: 4px; }
  .toc { background: var(--card); border: 1px solid var(--border); border-radius: 10px; padding: 14px 18px; margin: 0 0 28px; font-size: 14px; }
  .toc h2 { margin: 0 0 8px; font-size: 13px; text-transform: uppercase; letter-spacing: 0.05em; color: var(--muted); }
  .toc ul { margin: 0; padding-left: 18px; columns: 2; }
  .meta-line { color: var(--muted); font-size: 13.5px; margin: -8px 0 20px; }
  .actions a { display: inline-block; padding: 9px 16px; margin: 0 8px 8px 0; border-radius: 6px; text-decoration: none; font-weight: 600; font-size: 14px; border: 1px solid var(--border); color: var(--fg); background: var(--card); }
  .actions a.primary { background: var(--accent); color: #fff; border-color: var(--accent); }
  .hero { padding: 8px 0 8px; }
  .hero h1 { font-size: clamp(30px, 5vw, 44px); }
  .rejected { color: var(--bad); font-weight: 600; }
  .ran { color: var(--good); font-weight: 600; }
  .pair { display: grid; gap: 16px; grid-template-columns: 1fr 1fr; margin: 16px 0; }
  .pair > div h4 { margin: 0 0 8px; font-size: 13px; text-transform: uppercase; letter-spacing: 0.05em; color: var(--muted); }
  ul.tight { margin: 8px 0; padding-left: 22px; }
  ul.tight li { margin: 4px 0; }
  @media (max-width: 680px) { .pair { grid-template-columns: 1fr; } }
  footer { border-top: 1px solid var(--rule); padding: 28px 0 60px; color: var(--muted); font-size: 13.5px; }
  footer a { color: var(--muted); }
  footer .wrap { max-width: 1080px; }
  .theme-toggle {
    background: none; border: 1px solid var(--border); border-radius: 6px;
    color: var(--muted); font: inherit; font-size: 12.5px; padding: 5px 10px;
    cursor: pointer; margin-left: 8px; line-height: 1.4;
  }
  .theme-toggle:hover { color: var(--fg); border-color: var(--accent); }
  .src-line { display: flex; gap: 10px; flex-wrap: wrap; align-items: baseline; }
  .src-line a.secondary { color: var(--muted); font-size: 13px; }
  pre.lex-src { font-size: 13px; line-height: 1.6; }
  pre.lex-src .kw { color: var(--tok-kw); font-weight: 600; }
  pre.lex-src .ty { color: var(--tok-ty); }
  pre.lex-src .st { color: var(--tok-st); }
  pre.lex-src .cm { color: var(--tok-cm); font-style: italic; }
  pre.lex-src .nu { color: var(--tok-nu); }
  pre.lex-src .ef { color: var(--tok-ef); }
  @media (max-width: 680px) { h1 { font-size: 27px; } .toc ul { columns: 1; } .side-index { columns: 1; } }
  "
}

fn nav_item(href :: Str, label :: Str, key :: Str, active :: Str) -> Str {
  let cls := match key == active { true => "navlink active", false => "navlink" }
  "<a class=\"" + cls + "\" href=\"" + href + "\">" + label + "</a>\n"
}

fn nav_html(root :: Str, active :: Str) -> Str {
  "<nav class=\"topnav\"><div class=\"wrap\">\n" +
  "<a class=\"brand\" href=\"" + root + "index.html\">Lex docs</a>\n" +
  nav_item(root + "index.html", "Home", "home", active) +
  nav_item(root + "getting-started/index.html", "Getting Started", "start", active) +
  nav_item(root + "guides/index.html", "Guides", "guides", active) +
  nav_item(root + "vcs-hub/index.html", "VCS &amp; Hub", "vcs", active) +
  nav_item(root + "reference/index.html", "Reference", "reference", active) +
  "<a class=\"navlink gh\" href=\"https://github.com/alpibrusl/lex-lang\">GitHub ↗</a>\n" +
  "<button type=\"button\" id=\"theme-toggle\" class=\"theme-toggle\" aria-label=\"Toggle color theme (light, dark, or match system)\">auto</button>\n" +
  "</div></nav>\n" +
  theme_toggle_script()
}

# Read before first paint (called from `page()`, inside `<head>`, ahead of
# the `<style>` tag) so an explicit stored choice applies before the CSS
# cascade ever runs — the alternative is a visible flash to the "wrong"
# theme on load. Every localStorage access is wrapped in try/catch: a
# private-browsing window, a managed profile with storage blocked, or a
# non-browser preview tool can all make `localStorage` throw or simply
# not persist, and none of that should ever break the page — it just
# falls back to "auto" (the media query) for that load.
fn theme_key() -> Str { "lex-docs-theme" }

fn theme_head_script() -> Str {
  "<script>\n" +
  "(function(){\n" +
  "  try {\n" +
  "    var v = localStorage.getItem('" + theme_key() + "');\n" +
  "    if (v === 'dark' || v === 'light') {\n" +
  "      document.documentElement.setAttribute('data-theme', v);\n" +
  "    }\n" +
  "  } catch (e) {}\n" +
  "})();\n" +
  "</script>\n"
}

# The toggle button's own behavior: cycles light -> dark -> auto and
# persists the choice. `auto` is stored as "no key" (removed, not
# written as the literal string) so a visitor who has never touched the
# control and one who explicitly picked "auto" are indistinguishable —
# both fall through to the OS media query, which is the point of "auto".
# Emitted once per page, immediately after the nav markup that contains
# `#theme-toggle`, so `getElementById` always finds an already-parsed
# element (no `DOMContentLoaded` wait needed).
fn theme_toggle_script() -> Str {
  "<script>\n" +
  "(function(){\n" +
  "  var KEY = '" + theme_key() + "';\n" +
  "  var ORDER = ['light', 'dark', 'auto'];\n" +
  "  var btn = document.getElementById('theme-toggle');\n" +
  "  if (!btn) { return; }\n" +
  "  function readStored(){\n" +
  "    try { return localStorage.getItem(KEY); } catch (e) { return null; }\n" +
  "  }\n" +
  "  function writeStored(v){\n" +
  "    try {\n" +
  "      if (v === 'auto') { localStorage.removeItem(KEY); }\n" +
  "      else { localStorage.setItem(KEY, v); }\n" +
  "    } catch (e) {}\n" +
  "  }\n" +
  "  function label(v){\n" +
  "    if (v === 'dark') { return '☾ dark'; }\n" +
  "    if (v === 'light') { return '☀ light'; }\n" +
  "    return 'auto';\n" +
  "  }\n" +
  "  function apply(v){\n" +
  "    if (v === 'dark' || v === 'light') {\n" +
  "      document.documentElement.setAttribute('data-theme', v);\n" +
  "    } else {\n" +
  "      document.documentElement.removeAttribute('data-theme');\n" +
  "    }\n" +
  "    btn.textContent = label(v);\n" +
  "    btn.setAttribute('data-theme-choice', v);\n" +
  "  }\n" +
  "  var current = readStored();\n" +
  "  if (current !== 'dark' && current !== 'light') { current = 'auto'; }\n" +
  "  apply(current);\n" +
  "  btn.addEventListener('click', function(){\n" +
  "    var idx = ORDER.indexOf(current);\n" +
  "    current = ORDER[(idx + 1) % ORDER.length];\n" +
  "    writeStored(current);\n" +
  "    apply(current);\n" +
  "  });\n" +
  "})();\n" +
  "</script>\n"
}

fn footer_html() -> Str {
  "<footer><div class=\"wrap\">\n" +
  "Generated from the <code>alpibrusl/lex-lang</code> source on every push to <code>main</code> " +
  "(<a href=\"https://github.com/alpibrusl/lex-lang/blob/main/.github/workflows/docs.yml\">.github/workflows/docs.yml</a>), " +
  "by a Lex program (<a href=\"https://github.com/alpibrusl/lex-lang/tree/main/sitegen\">sitegen/generate.lex</a>). " +
  "Lex is EUPL-1.2 licensed. · " +
  "<a href=\"https://github.com/alpibrusl/lex-lang/blob/main/CHANGELOG.md\">Changelog</a> · " +
  "<a href=\"https://lexlang.org\">lexlang.org</a>\n" +
  "</div></footer>\n"
}

# `root` is the relative path back to the site root (e.g. "" at the top
# level, "../" one level down, "../../" two levels down) so every page
# can be written with plain relative links and still work however deep
# it's nested.
fn page(root :: Str, title :: Str, description :: Str, active :: Str, wide :: Bool, body :: Str) -> Str {
  let main_class := match wide { true => "wrap wide", false => "wrap" }
  "<!doctype html>\n<html lang=\"en\">\n<head>\n" +
  "<meta charset=\"utf-8\">\n" +
  theme_head_script() +
  "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n" +
  "<title>" + title + " — Lex docs</title>\n" +
  "<meta name=\"description\" content=\"" + description + "\">\n" +
  "<link rel=\"canonical\" href=\"https://doc.lexlang.org/\">\n" +
  "<style>" + base_css() + "</style>\n" +
  "</head>\n<body>\n" +
  nav_html(root, active) +
  "<main class=\"" + main_class + "\">\n" + body + "\n</main>\n" +
  footer_html() +
  "</body>\n</html>\n"
}
