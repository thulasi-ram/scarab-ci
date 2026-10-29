"""Extract cel-spec conformance cases from a cel-rust checkout into JSON lines.

cel-rust (github.com/cel-rust/cel-rust) generates one Rust test per cel-spec
SimpleTest under conformance/tests/gen/, each embedding the test's textproto.
This reads those files (no network, no protobuf toolchain) and writes, per
case: file, section, name, expr, check_only, disable_check, whether it declares
a type_env / bindings / container, and its expected result: a value's kind, an
eval error, or a typed_result's deduced type rendered as `list<int>` etc.

    python3 -I extract.py <cel-rust>/conformance/tests/gen > cases.jsonl
"""
import json
import pathlib
import re
import sys

TEST = re.compile(
    r'// Section: (?P<section>\S+)|'
    r'// Test: (?P<name>[^\n]+?)\s*\n(?:\s*#\[[^\n]*\]\s*\n)*\s*fn [\w#]+\(\) \{\s*'
    r'run_test\(&dedent!\(\s*r(?P<hashes>#+)"(?P<body>.*?)"(?P=hashes)\s*\)',
    re.S,
)
TOKEN = re.compile(r'\s*(?:(#[^\n]*)|("(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])*\')|([{}:\[\],<>])|([^\s{}:\[\],<>"\']+))')


def unescape(s):
    """Textproto string literal (quotes included) to text."""
    body, out, i = s[1:-1], bytearray(), 0
    simple = {'n': 10, 't': 9, 'r': 13, '"': 34, "'": 39, '\\': 92, 'a': 7, 'b': 8, 'f': 12, 'v': 11, '?': 63}
    while i < len(body):
        c = body[i]
        if c != '\\':
            out += c.encode()
            i += 1
            continue
        n = body[i + 1]
        if n in simple:
            out.append(simple[n]); i += 2
        elif n in '01234567':
            j = i + 1
            while j < len(body) and j < i + 4 and body[j] in '01234567':
                j += 1
            out.append(int(body[i + 1:j], 8)); i = j
        elif n in 'xX':
            j = i + 2
            while j < len(body) and j < i + 4 and body[j] in '0123456789abcdefABCDEF':
                j += 1
            out.append(int(body[i + 2:j], 16)); i = j
        elif n == 'u':
            out += chr(int(body[i + 2:i + 6], 16)).encode(); i += 6
        elif n == 'U':
            out += chr(int(body[i + 2:i + 10], 16)).encode(); i += 10
        else:
            out += ('\\' + n).encode(); i += 2
    return out.decode('utf-8', 'replace')


def tokens(text):
    pos = 0
    while pos < len(text):
        m = TOKEN.match(text, pos)
        if not m or m.end() == pos:
            break
        pos = m.end()
        if m.group(1):
            continue
        tok = m.group(2) or m.group(3) or m.group(4)
        if tok:
            yield tok


def is_str(tok):
    return tok[0] in '"\''


class Toks:
    def __init__(self, text):
        self.toks, self.pos = list(tokens(text)), 0

    def next(self):
        tok = self.toks[self.pos]
        self.pos += 1
        return tok

    def scalar(self):
        """A scalar; adjacent string literals concatenate, as in C."""
        tok = self.next()
        if not is_str(tok):
            return tok
        parts = [unescape(tok)]
        while self.pos < len(self.toks) and is_str(self.toks[self.pos]):
            parts.append(unescape(self.next()))
        return '"' + ''.join(parts).replace('\\', '\\\\').replace('"', '\\"') + '"'


def parse(toks, close=None):
    """Textproto message to a list of (field, value) pairs; scalars stay strings."""
    fields = []
    while toks.pos < len(toks.toks):
        name = toks.next()
        if name == close:
            return fields
        if name in (',', ';'):
            continue
        nxt = toks.toks[toks.pos]
        if nxt == ':':
            toks.next()
            nxt = toks.toks[toks.pos]
        if nxt in ('{', '<'):
            toks.next()
            fields.append((name, parse(toks, '}' if nxt == '{' else '>')))
        elif nxt == '[':
            toks.next()
            while True:
                t = toks.toks[toks.pos]
                if t == ']':
                    toks.next()
                    break
                if t == ',':
                    toks.next()
                    continue
                if t == '{':
                    toks.next()
                    fields.append((name, parse(toks, '}')))
                else:
                    fields.append((name, toks.scalar()))
        else:
            fields.append((name, toks.scalar()))
    return fields


def get(fields, name):
    return next((v for k, v in fields if k == name), None)


PRIMS = {'BOOL': 'bool', 'INT64': 'int', 'UINT64': 'uint', 'DOUBLE': 'double', 'STRING': 'string', 'BYTES': 'bytes'}


def render_type(t):
    k, v = t[0]
    if k == 'primitive':
        return PRIMS.get(v, v.lower())
    if k == 'list_type':
        return 'list<%s>' % render_type(get(v, 'elem_type'))
    if k == 'map_type':
        return 'map<%s, %s>' % (render_type(get(v, 'key_type')), render_type(get(v, 'value_type')))
    if k == 'dyn':
        return 'dyn'
    if k == 'null':
        return 'null'
    if k == 'well_known':
        return {'TIMESTAMP': 'timestamp', 'DURATION': 'duration'}.get(v, v.lower())
    return '%s(%s)' % (k, v if isinstance(v, str) else '...')


VALUE_KINDS = {'int64_value': 'int', 'uint64_value': 'uint', 'double_value': 'double', 'string_value': 'string',
               'bytes_value': 'bytes', 'bool_value': 'bool', 'null_value': 'null', 'list_value': 'list',
               'map_value': 'map', 'enum_value': 'enum', 'type_value': 'type'}


def value_kind(v):
    if not v:
        return 'unknown'
    k, inner = v[0]
    if k == 'object_value':
        url = get(inner, '@type') or ''
        text = json.dumps(inner)
        if 'Timestamp' in text:
            return 'timestamp'
        if 'Duration' in text:
            return 'duration'
        return 'object'
    return VALUE_KINDS.get(k, k)


def main(gen_dir):
    for path in sorted(pathlib.Path(gen_dir).glob('*.rs')):
        section = None
        for m in TEST.finditer(path.read_text()):
            if m.group('section'):
                section = m.group('section')
                continue
            f = parse(Toks(m.group('body')))
            case = {
                'file': path.stem, 'section': section, 'name': m.group('name'),
                'expr': unescape(get(f, 'expr')),
                'check_only': get(f, 'check_only') == 'true',
                'disable_check': get(f, 'disable_check') == 'true',
                'type_env': get(f, 'type_env') is not None,
                'bindings': get(f, 'bindings') is not None,
                'container': get(f, 'container') is not None,
            }
            if get(f, 'typed_result') is not None:
                tr = get(f, 'typed_result')
                case['result'] = 'typed'
                case['deduced_type'] = render_type(get(tr, 'deduced_type'))
                case['value_kind'] = value_kind(get(tr, 'result')) if get(tr, 'result') else None
            elif get(f, 'value') is not None:
                case['result'] = 'value'
                case['value_kind'] = value_kind(get(f, 'value'))
            elif get(f, 'eval_error') is not None or get(f, 'any_eval_errors') is not None:
                case['result'] = 'eval_error'
            elif get(f, 'unknown') is not None or get(f, 'any_unknowns') is not None:
                case['result'] = 'unknown'
            else:
                case['result'] = 'value'
                case['value_kind'] = 'bool'  # SimpleTest defaults to `true`
            print(json.dumps(case))


if __name__ == '__main__':
    main(sys.argv[1])
