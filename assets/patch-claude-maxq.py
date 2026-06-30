#!/usr/bin/env python3
"""Build a patched copy of the Claude Code binary that raises AskUserQuestion bounds.

    patch-claude-maxq.py [QUESTIONS] [OPTIONS] [DEST]
        QUESTIONS  max questions per call   (1..99, default 10)
        OPTIONS    max options per question (2..99, default 10)
        DEST       output path (default ~/.local/bin/claude-maxq)

Reads the CURRENT stock binary (resolving the symlink) so it is safe to re-run
after Claude Code updates. All edits preserve byte length so Mach-O offsets do
not shift; the copy is re-signed ad-hoc (the stock signature breaks on any edit).
Set CLAUDE_BIN to override binary detection.
"""
import os, re, shutil, subprocess, sys

HOME = os.path.expanduser("~")

def find_src():
    cands = []
    if os.environ.get("CLAUDE_BIN"):
        cands.append(os.environ["CLAUDE_BIN"])
    cands.append(os.path.join(HOME, ".local/bin/claude"))
    w = shutil.which("claude")
    if w:
        cands.append(w)
    for c in cands:
        if c and os.path.exists(c):
            return os.path.realpath(c)
    raise SystemExit("could not locate the claude binary; set CLAUDE_BIN")

def pad(base, n, fill=b" "):
    if len(base) > n:
        raise SystemExit(f"replacement too long ({len(base)} > {n}): {base!r}")
    return base + fill * (n - len(base))

def validator_repl(orig, value):
    minpart = orig[:orig.index(b".max(")]
    cand = minpart + b".max(%d)" % value
    if len(cand) > len(orig):
        cand = b".max(%d)" % value
    return pad(cand, len(orig))

def patch_region(data, full_pattern, cap_pattern, value, new_long, new_short, label):
    """Raise the `.max(4)` cap (REQUIRED) and refresh the tool description text
    (BEST-EFFORT). Only the cap bump matters functionally; the description is
    cosmetic. If Claude's bundle changes the `.describe(...)` shape we still bump
    the cap and leave the text as-is, so a minor reshape no longer breaks us.
    Raises only if the cap anchor itself can't be located."""
    full = list(re.compile(full_pattern, re.S).finditer(data))
    if len(full) > 1:
        raise SystemExit(f"{label}: ambiguous describe match ({len(full)})")
    if len(full) == 1:
        m = full[0]
        prefix, validator, d_open, longstr, sep, shortstr, close = m.groups()
        rebuilt = (prefix + validator_repl(validator, value) + d_open
                   + pad(new_long, len(longstr)) + sep
                   + pad(new_short, len(shortstr)) + close)
        if len(rebuilt) != len(m.group(0)):
            raise SystemExit(f"{label}: length mismatch")
        data[m.start():m.end()] = rebuilt
        print(f"  {label}: .max(4) -> .max({value}) (+ description)")
        return
    # describe form unrecognised -> bump the cap only, leave the text untouched.
    caps = list(re.compile(cap_pattern, re.S).finditer(data))
    if len(caps) != 1:
        raise SystemExit(f"{label}: cap anchor expected 1 match, found {len(caps)}")
    m = caps[0]
    prefix, validator = m.groups()
    rebuilt = prefix + validator_repl(validator, value)
    if len(rebuilt) != len(m.group(0)):
        raise SystemExit(f"{label}: length mismatch (cap-only)")
    data[m.start():m.end()] = rebuilt
    print(f"  {label}: .max(4) -> .max({value}) (description left as-is)")

def main():
    q = int(sys.argv[1]) if len(sys.argv) > 1 else 10
    a = int(sys.argv[2]) if len(sys.argv) > 2 else 10
    dst = sys.argv[3] if len(sys.argv) > 3 else os.path.join(HOME, ".local/bin/claude-maxq")
    if not (1 <= q <= 99 and 2 <= a <= 99):
        raise SystemExit("QUESTIONS must be 1..99 and OPTIONS must be 2..99")
    src = find_src()
    os.makedirs(os.path.dirname(dst) or ".", exist_ok=True)
    print(f"source : {src}")
    print(f"target : {dst}  (questions={q}, options={a})")

    # Build into a temp file and only os.replace() into DST on full success, so a
    # failed/partial patch can never leave a poisoned (unpatched) binary behind
    # that a caller would then trust. On any error we delete the temp and re-raise.
    tmp = f"{dst}.tmp.{os.getpid()}"
    try:
        shutil.copyfile(src, tmp)
        shutil.copymode(src, tmp)
        data = bytearray(open(tmp, "rb").read())

        patch_region(
            data,
            rb'([\w$]+\.array\([\w$]+\(\)\))(\.min\(1\)\.max\(4\))(\.describe\([\w$]+\(\)\?")(.*?)(":")(.*?)("\),)',
            rb'([\w$]+\.array\([\w$]+\(\)\))(\.min\(1\)\.max\(4\))',
            q,
            b"Questions to ask the user (1-%d questions). You may ask up to %d "
            b"questions in a single call; each question has 2-%d options." % (q, q, a),
            b"Questions to ask the user (1-%d)" % q,
            "questions",
        )
        opt = (b"The available choices for this question (2-%d options). Each option "
               b"should be a distinct, mutually exclusive choice (unless multiSelect "
               b"is enabled). No 'Other' option - it is added automatically." % a)
        patch_region(
            data,
            rb'(options:[\w$]+\.array\([\w$]+\(\)\))(\.min\(2\)\.max\(4\))(\.describe\([\w$]+\(\)\?")(.*?)(":")(.*?)("\),)',
            rb'(options:[\w$]+\.array\([\w$]+\(\)\))(\.min\(2\)\.max\(4\))',
            a, opt, opt, "options ",
        )

        open(tmp, "wb").write(data)
        os.chmod(tmp, 0o755)
        print("re-signing ad-hoc...")
        subprocess.run(["xattr", "-cr", tmp], check=False)
        subprocess.run(["codesign", "--remove-signature", tmp], check=False)
        if subprocess.run(["codesign", "--force", "--sign", "-", tmp]).returncode != 0:
            raise SystemExit("codesign failed")
        # Smoke-test the patched binary BEFORE publishing it as the cache.
        v = subprocess.run([tmp, "--version"], capture_output=True, text=True)
        if v.returncode != 0:
            raise SystemExit("patched binary failed to run:\n" + v.stderr.strip())
        os.replace(tmp, dst)  # atomic publish: only reached on full success
        print(f"OK -> {dst}  ({v.stdout.strip()})")
    except BaseException:
        try:
            os.remove(tmp)
        except OSError:
            pass
        raise

if __name__ == "__main__":
    main()
