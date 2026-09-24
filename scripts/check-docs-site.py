#!/usr/bin/env python3
"""Fail CI if the generated docs site (`_site/`, from sitegen/generate.lex)
is missing expected pages, has unbalanced HTML tags (a sign the Markdown
converter mis-rendered a doc comment or a docs/*.md file), a broken
relative link/anchor, or a leftover internal placeholder token.

This is the "keep it updated automatically, never drifts" gate for #567:
without it, a `sitegen/*.lex` or `docs/*.md` change that breaks generation
would only be noticed by looking at the deployed site, not in CI.
"""
import html.parser
import os
import re
import sys

SITE = "_site"

REQUIRED_FILES = [
    "index.html",
    "CNAME",
    "api-docs.json",
    "getting-started/index.html",
    "guides/index.html",
    "vcs-hub/index.html",
    "reference/index.html",
    "reference/roadmap.html",
    "reference/status.html",
    "reference/invariants.html",
    "reference/migrating-0-10.html",
    "reference/cross-compile.html",
    "reference/deploy.html",
    "reference/effect-row-polymorphism.html",
]

VOID_TAGS = {"br", "hr", "img", "meta", "link", "input"}


class TagChecker(html.parser.HTMLParser):
    def __init__(self):
        super().__init__()
        self.stack = []
        self.errors = []

    def handle_starttag(self, tag, attrs):
        if tag not in VOID_TAGS:
            self.stack.append(tag)

    def handle_endtag(self, tag):
        if tag in VOID_TAGS:
            return
        if not self.stack or self.stack[-1] != tag:
            self.errors.append(f"mismatched </{tag}> (stack: {self.stack[-3:]})")
        else:
            self.stack.pop()


def fail(msg):
    print(f"::error::{msg}")
    global ok
    ok = False


ok = True


def main():
    if not os.path.isdir(SITE):
        fail(f"{SITE}/ does not exist — did sitegen/generate.lex run?")
        sys.exit(1)

    for rel in REQUIRED_FILES:
        if not os.path.isfile(os.path.join(SITE, rel)):
            fail(f"missing expected page: {SITE}/{rel}")

    cname_path = os.path.join(SITE, "CNAME")
    if os.path.isfile(cname_path):
        cname = open(cname_path).read().strip()
        if cname != "doc.lexlang.org":
            fail(f"CNAME is {cname!r}, expected 'doc.lexlang.org'")

    html_files = []
    for root, _dirs, files in os.walk(SITE):
        for f in files:
            if f.endswith(".html"):
                html_files.append(os.path.join(root, f))

    if len(html_files) < 30:
        fail(f"only {len(html_files)} HTML pages generated — expected at least 30 "
             f"(guides + reference + landing + getting-started + vcs-hub)")

    page_ids = {}
    for path in html_files:
        src = open(path, encoding="utf-8").read()

        if "LXDOCCODE" in src:
            fail(f"{path}: leftover internal code-span placeholder — "
                 f"md.lex's extract_code/restore_code pairing broke")

        checker = TagChecker()
        try:
            checker.feed(src)
        except Exception as e:  # pragma: no cover
            fail(f"{path}: HTML parse error: {e}")
            continue
        if checker.errors:
            fail(f"{path}: unbalanced tags: {checker.errors[:5]}")
        if checker.stack:
            fail(f"{path}: unclosed tags at EOF: {checker.stack}")

        page_ids[path] = set(re.findall(r'id="([^"]+)"', src))

    for path in html_files:
        src = open(path, encoding="utf-8").read()
        base_dir = os.path.dirname(path)
        for m in re.finditer(r'href="([^"]+)"', src):
            href = m.group(1)
            if href.startswith("http") or href.startswith("mailto:"):
                continue
            file_part, _, frag = href.partition("#")
            if file_part:
                target = os.path.normpath(os.path.join(base_dir, file_part))
                if not os.path.isfile(target):
                    fail(f"{path}: broken relative link to {href!r} (resolved: {target})")
                    continue
                target_ids = page_ids.get(target, set())
            else:
                target = path
                target_ids = page_ids.get(path, set())
            if frag and frag not in target_ids:
                fail(f"{path}: broken anchor {href!r} — no id=\"{frag}\" on target page")

    if ok:
        print(f"docs site OK: {len(html_files)} pages, all links and anchors resolve, "
              f"no unbalanced tags, no leftover markers.")
    else:
        sys.exit(1)


if __name__ == "__main__":
    main()
