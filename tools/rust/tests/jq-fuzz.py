"""Random jq programs, run by our jq and the original (built from jq's development tree); any
difference in output, messages or exit status is printed with the program (except that, of
a program jq cannot compile, only the first error is compared: see recovery_only). The generators,
errors and backtracking in them check the order of evaluation (which outputs come first, what
an error interrupts), the hardest part to get the same.

    python3 jq-fuzz.py OUR_JQ REF_JQ [COUNT] [SEED]"""
import random, subprocess, sys

OURS, REF = sys.argv[1], sys.argv[2]
COUNT = int(sys.argv[3]) if len(sys.argv) > 3 else 2000
rnd = random.Random(int(sys.argv[4]) if len(sys.argv) > 4 else 1)

INPUT = '{"a":1,"b":[1,2,{"c":"x"}],"s":"ab","n":null,"t":true,"o":{"x":1,"y":[3,4]}}'


def atom():
    return rnd.choice([
        ".", ".a", ".b", ".b[0]", ".b[]", ".o", ".o[]", ".s", ".n", ".t", ".x", "1", "2", "\"s\"", "null", "true", "false",
        "[]", "{}", "(1,2)", "empty", "error(\"e\")", "error", "$v", "$__loc__", ".[]?", ".b[-1]", ".o.y[1:]", "(.b|length)",
        "range(3)", "[range(2)]", "input?", "env|type", "nan", "infinite", "-1", "0.5", "\"\\(1,2)\"", "@base64", "keys?",
    ])


def expr(d=0):
    if d > 3 or rnd.random() < 0.25:
        return atom()
    a = lambda: expr(d + 1)
    k = rnd.randrange(34)
    return [
        lambda: f"{a()} | {a()}", lambda: f"{a()}, {a()}", lambda: f"{a()} + {a()}", lambda: f"{a()} - {a()}",
        lambda: f"{a()} * {a()}", lambda: f"{a()} / {a()}", lambda: f"{a()} % {a()}", lambda: f"{a()} == {a()}",
        lambda: f"{a()} < {a()}", lambda: f"{a()} and {a()}", lambda: f"{a()} or {a()}", lambda: f"{a()} // {a()}",
        lambda: f"[{a()}]", lambda: f"{{a: {a()}, ({a()}|tostring): {a()}}}", lambda: f"if {a()} then {a()} else {a()} end",
        lambda: f"if {a()} then {a()} end", lambda: f"try {a()}", lambda: f"try {a()} catch {a()}", lambda: f"({a()})?",
        lambda: f"reduce {a()} as $x ({a()}; . + $x)", lambda: f"foreach {a()} as $x (0; . + 1; [$x, .])",
        lambda: f"label $out | {a()} | ., break $out", lambda: f"{a()} as $v | {a()}", lambda: f"{a()} as [$v] ?// $v | $v",
        lambda: f"path({a()})", lambda: f"[paths]", lambda: f"first({a()})", lambda: f"[limit(2; {a()})]",
        lambda: f".a = {a()}", lambda: f".b[] |= {a()}", lambda: f".a += {a()}", lambda: f"del(.b[0], .o.x)",
        lambda: f"def f: {a()}; [f, f]", lambda: f"def g(h): [h, h]; g({a()})",
    ][k]()


def run(binary, prog):
    try:
        r = subprocess.run([binary, "-c", "--arg", "v", "V", prog], input=(INPUT + " 5 \"z\"").encode(), capture_output=True, timeout=20)
    except subprocess.TimeoutExpired:
        return ("timeout",)
    return r.returncode, r.stdout, r.stderr.replace(binary.encode(), b"jq")


def recovery_only(a, b):
    """A compile error both report first, after which bison's error recovery found more (its
    later errors are not reproduced: ours stops at the first one, with the notes jq's grammar
    adds right away)."""
    if a[0] != 3 or b[0] != 3:
        return False
    first = lambda e: e.split(b"\njq: ")[0]
    return first(a[2]) == first(b[2])


bad = recov = 0
for i in range(COUNT):
    p = expr()
    a, b = run(OURS, p), run(REF, p)
    if a != b:
        if recovery_only(a, b):
            recov += 1
            continue
        bad += 1
        print(f"DIFF {p!r}\n  ours: {a!r}\n  ref:  {b!r}")
print(f"{COUNT - bad - recov} of {COUNT} programs identical, {recov} differ only in errors after the first syntax error")
sys.exit(1 if bad else 0)
