#!/usr/bin/env python3
"""citecheck: check every link in handover Markdown pages.

Usage, from the repository root:

    python3 docs/handover/tools/citecheck.py [--quiet] docs/handover/*.md

The commits the pages cite must be in your local clones. A commit that only
an open or merged pull request holds may need fetching first, for example
`git fetch origin <full-sha>` (GitHub serves reachable commits by SHA).
Links into the other repository (cw-message-gateway <-> rustos) are checked
against a clone of it beside this one (../rustos or ../cw-message-gateway),
or wherever CW_GATEWAY_DIR / RUSTOS_DIR point.

For each GitHub permalink to robinonsay/cw-message-gateway or robinonsay/rustos
(https://github.com/robinonsay/<repo>/blob/<sha>/<path>#L<a>[-L<b>]) it checks
that the commit exists in the local clone, the file exists at that commit and
the line range is inside the file, and prints the cited lines so a reader can
compare them with the sentence that cites them.

Relative links (other pages, diagrams, anchors) are checked for existence.
Links to a moving ref (blob/main, blob/master, tree/main) are flagged: the
handover cites fixed commits only. The one exception is a link to the other
repository's handover set (docs/handover/), on its default branch or on the
docs branch.

Exit status 1 if anything is broken.
"""
import os
import re
import subprocess
import sys

_HERE = os.path.dirname(os.path.abspath(__file__))
_ROOT = os.path.normpath(os.path.join(_HERE, "..", "..", ".."))  # docs/handover/tools -> repo root


def _repo_dir(name, env):
    if os.environ.get(env):
        return os.environ[env]
    if os.path.basename(_ROOT) == name or (name == "cw-message-gateway" and os.path.exists(os.path.join(_ROOT, "crates", "hfnode"))):
        return _ROOT
    if name == "rustos" and os.path.exists(os.path.join(_ROOT, "firmware", "pico2", "link.ld")):
        return _ROOT
    return os.path.join(os.path.dirname(_ROOT), name)


REPOS = {
    "cw-message-gateway": _repo_dir("cw-message-gateway", "CW_GATEWAY_DIR"),
    "ic-7300-hf-server": _repo_dir("cw-message-gateway", "CW_GATEWAY_DIR"),
    "rustos": _repo_dir("rustos", "RUSTOS_DIR"),
}
REFDEF = re.compile(r"^\s{0,3}\[([^\]]+)\]:\s*<?([^>\s]+)>?")
AUTOLINK = re.compile(r"<(https://github\.com/[^>\s]+)>")
LINK = re.compile(r"\[([^\]]*)\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
GH = re.compile(
    r"https://github\.com/robinonsay/([\w.-]+)/(blob|tree)/([0-9a-f]{7,40}|[\w./-]+?)/([^#?]+?)(?:#L(\d+)(?:-L(\d+))?)?$"
)
MOVING = {"main", "master", "HEAD"}
DOCS_BRANCH = "claude/handover-docs-us00cn"
# Refs a link to the other repository's handover set may use: its default branch
# (where the set lives once merged) or the docs branch it was written on.
HANDOVER_REFS = (DOCS_BRANCH, "main", "master")
_cache = {}


def git_show(repo, sha, path):
    key = (repo, sha, path)
    if key not in _cache:
        r = subprocess.run(
            ["git", "-C", repo, "show", f"{sha}:{path}"],
            capture_output=True,
        )
        _cache[key] = r.stdout.decode("utf-8", "replace").split("\n") if r.returncode == 0 else None
    return _cache[key]


def slug(h):
    h = h.strip().lower()
    h = re.sub(r"[^\w\- ]", "", h)
    return h.replace(" ", "-")


def anchors(md_path):
    out = set()
    try:
        text = open(md_path, encoding="utf-8").read()
    except OSError:
        return out
    in_code = False
    counts = {}
    for line in text.split("\n"):
        if line.startswith("```"):
            in_code = not in_code
            continue
        if in_code:
            continue
        m = re.match(r"^(#{1,6})\s+(.*?)\s*#*\s*$", line)
        if m:
            s = slug(re.sub(r"`|\*|\[([^\]]*)\]\([^)]*\)", r"\1", m.group(2)))
            n = counts.get(s, 0)
            counts[s] = n + 1
            out.add(s if n == 0 else f"{s}-{n}")
    for m in re.finditer(r"<a\s+(?:name|id)=\"([^\"]+)\"", text):
        out.add(m.group(1))
    return out


def main(argv):
    quiet = "--quiet" in argv
    files = [a for a in argv if not a.startswith("--")]
    bad = 0
    total = 0
    for md in files:
        base = os.path.dirname(os.path.abspath(md))
        text = open(md, encoding="utf-8").read()
        # Skip fenced code blocks.
        stripped = re.sub(r"```.*?```", lambda m: "\n" * m.group(0).count("\n"), text, flags=re.S)
        print(f"== {md}")
        for lineno, line in enumerate(stripped.split("\n"), 1):
            found = [(m.group(1), m.group(2)) for m in LINK.finditer(line)]
            rd = REFDEF.match(line)
            if rd:
                found.append((rd.group(1), rd.group(2)))
            found += [("autolink", m.group(1)) for m in AUTOLINK.finditer(line)]
            for label, url in found:
                total += 1
                if url.startswith("mailto:"):
                    continue
                g = GH.match(url)
                if g:
                    repo_name, kind, ref, path, a, b = g.groups()
                    repo = REPOS.get(repo_name)
                    tail = url.split("/blob/", 1)[-1] if "/blob/" in url else url.split("/tree/", 1)[-1]
                    docs_ref = next((r for r in HANDOVER_REFS if tail.startswith(r + "/docs/handover")), None)
                    if docs_ref:
                        # The one allowed moving link: the other repo's handover set, on its
                        # default branch once merged there, or on the docs branch before.
                        rel = tail[len(docs_ref) + 1 :].split("#")[0]
                        there = repo and (
                            os.path.exists(os.path.join(repo, rel))
                            or not subprocess.run(["git", "-C", repo, "cat-file", "-e", f"origin/{docs_ref}:{rel}"], capture_output=True).returncode
                        )
                        if there:
                            if not quiet:
                                print(f"  ok   {md}:{lineno} handover set {repo_name}@{docs_ref}:{rel}")
                        else:
                            print(f"  BAD  {md}:{lineno} handover-set target missing locally: {repo_name}@{docs_ref}:{rel}")
                            bad += 1
                        continue
                    if ref in MOVING or not re.fullmatch(r"[0-9a-f]{7,40}", ref or ""):
                        print(f"  BAD  {md}:{lineno} moving ref '{ref}' in {url}")
                        bad += 1
                        continue
                    if repo is None:
                        print(f"  ?    {md}:{lineno} unknown repo {repo_name}")
                        continue
                    if kind == "tree":
                        r = subprocess.run(["git", "-C", repo, "cat-file", "-e", f"{ref}:{path.rstrip('/')}"], capture_output=True)
                        if r.returncode:
                            print(f"  BAD  {md}:{lineno} no such tree {ref}:{path}")
                            bad += 1
                        elif not quiet:
                            print(f"  ok   {md}:{lineno} tree {ref[:7]}:{path}")
                        continue
                    if subprocess.run(["git", "-C", repo, "cat-file", "-e", f"{ref}^{{commit}}"], capture_output=True).returncode:
                        print(f"  BAD  {md}:{lineno} commit {ref[:12]} is not in {repo} (try: git -C {repo} fetch origin {ref})")
                        bad += 1
                        continue
                    lines = git_show(repo, ref, path)
                    if lines is None:
                        print(f"  BAD  {md}:{lineno} no file {path} at {ref[:7]} ({repo_name})")
                        bad += 1
                        continue
                    if a:
                        a = int(a)
                        b = int(b) if b else a
                        n = len(lines) - (1 if lines and lines[-1] == "" else 0)
                        if a < 1 or b > n or b < a:
                            print(f"  BAD  {md}:{lineno} {path}#L{a}-L{b} outside 1..{n} at {ref[:7]}")
                            bad += 1
                            continue
                        if not quiet:
                            shown = lines[a - 1 : min(b, a + 5)]
                            print(f"  ok   {md}:{lineno} [{label}] -> {repo_name}@{ref[:7]} {path}:{a}" + (f"-{b}" if b != a else ""))
                            for i, t in enumerate(shown):
                                print(f"         {a + i:>5}| {t[:150]}")
                            if b - a >= 6:
                                print(f"         ... ({b - a + 1} lines)")
                    elif not quiet:
                        print(f"  ok   {md}:{lineno} [{label}] -> {repo_name}@{ref[:7]} {path} (whole file)")
                    continue
                if re.match(r"^[a-z]+://", url):
                    if "github.com/robinonsay" in url and re.search(r"/(blob|tree)/(main|master)/", url):
                        print(f"  BAD  {md}:{lineno} moving ref in {url}")
                        bad += 1
                    elif not quiet:
                        print(f"  ext  {md}:{lineno} {url}")
                    continue
                # Relative link.
                target, _, frag = url.partition("#")
                tpath = os.path.normpath(os.path.join(base, target)) if target else os.path.abspath(md)
                if not os.path.exists(tpath):
                    print(f"  BAD  {md}:{lineno} missing relative target {url}")
                    bad += 1
                    continue
                if frag and tpath.endswith(".md") and not re.fullmatch(r"L\d+(-L\d+)?", frag):
                    if frag not in anchors(tpath):
                        print(f"  BAD  {md}:{lineno} no heading anchor #{frag} in {os.path.relpath(tpath, base)}")
                        bad += 1
                        continue
                if not quiet:
                    print(f"  ok   {md}:{lineno} rel {url}")
        # Images referenced with ![...](...) are links too; the regex above already caught them.
    print(f"checked {total} links, {bad} broken")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
