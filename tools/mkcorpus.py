# Synthetic stress files for the engine tests, with file selection (spike/mkcorpus.py makes all three
# at once, always including the 1.5 GB big.pdf; spike/ is frozen, so this is a copy with a picker).
#   usage: python3 tools/mkcorpus.py <outdir> [lines] [set] [big] [heavy]   (default: lines set; big/heavy only if named)
#   lines.pdf one A0 sheet with 1,000,000 stroked lines (Flate, ~11 MB)
#   heavy.pdf one A0 sheet with 10,000,000 lines, each its own stroked path (Flate level 1); worker cancel test
#   set.pdf   2,000 A1 pages, 3,000 lines and a title block each (Flate, ~65 MB)
#   big.pdf   ~1.5 GB: 300 pages, each an uncompressed 1400x1250 RGB noise image (5 MB a page)
# Seed 7 as in spike. When lines and set are both made, they are byte-identical to the spike's (the
# random stream is shared in that order); set alone differs, because the random stream starts fresh.
# The output goes in corpus/ at the repo root, which is git-ignored: test files never get committed.
import os, random, sys, zlib
class W:
    def __init__(s, path): s.f = open(path, 'wb'); s.off = []; s.f.write(b'%PDF-1.7\n%\xe2\xe3\xcf\xd3\n')
    def obj(s, body, stream=None):
        s.off.append(s.f.tell()); n = len(s.off)
        if stream is None: s.f.write(b'%d 0 obj\n%s\nendobj\n' % (n, body))
        else: s.f.write(b'%d 0 obj\n%s\nstream\n' % (n, body)); s.f.write(stream); s.f.write(b'\nendstream\nendobj\n')
        return n
    def reserve(s): s.off.append(None); return len(s.off)
    def fill(s, n, body): s.off[n - 1] = s.f.tell(); s.f.write(b'%d 0 obj\n%s\nendobj\n' % (n, body))
    def close(s, root):
        x = s.f.tell(); s.f.write(b'xref\n0 %d\n0000000000 65535 f \n' % (len(s.off) + 1))
        for o in s.off: s.f.write(b'%010d 00000 n \n' % o)
        s.f.write(b'trailer\n<< /Size %d /Root %d 0 R >>\nstartxref\n%d\n%%%%EOF\n' % (len(s.off) + 1, root, x)); s.f.close()
def doc(path, pages):  # pages: list of (w, h, mk) where mk(writer) -> (content bytes, extra resources bytes)
    w = W(path); cat = w.reserve(); tree = w.reserve(); font = w.obj(b'<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>'); kids = []
    for (pw, ph, mk) in pages:
        content, res = mk(w)
        c = w.obj(b'<< /Length %d /Filter /FlateDecode >>' % len(content), content)
        kids.append(w.obj(b'<< /Type /Page /Parent %d 0 R /MediaBox [0 0 %d %d] /Contents %d 0 R /Resources << /Font << /F1 %d 0 R >> %s >> >>' % (tree, pw, ph, c, font, res)))
    w.fill(tree, b'<< /Type /Pages /Count %d /Kids [%s] >>' % (len(kids), b' '.join(b'%d 0 R' % k for k in kids)))
    w.fill(cat, b'<< /Type /Catalog /Pages %d 0 R >>' % tree); w.close(cat)
def lines(n, pw, ph, rnd):
    out = []
    for i in range(n):
        if i % 1000 == 0: out.append(b'S %.2f w %.2f %.2f %.2f RG\n' % (rnd.choice((0.1, 0.25, 0.5)), rnd.random() * .5, rnd.random() * .5, rnd.random() * .5))
        x, y = rnd.random() * pw, rnd.random() * ph
        out.append(b'%.1f %.1f m %.1f %.1f l\n' % (x, y, x + rnd.uniform(-40, 40), y + rnd.uniform(-40, 40)))
    out.append(b'S\n'); return b''.join(out)
if len(sys.argv) < 2:
    sys.exit('usage: mkcorpus.py <outdir> [lines] [set] [big]')
out = sys.argv[1]; want = sys.argv[2:] or ['lines', 'set']
bad = [n for n in want if n not in ('lines', 'set', 'big', 'heavy')]
if bad: sys.exit('unknown file(s): %s (choose from lines, set, big, heavy)' % ' '.join(bad))
os.makedirs(out, exist_ok=True)
rnd = random.Random(7)
def mk_lines(w):
    return zlib.compress(lines(1_000_000, 3370, 2384, rnd), 6), b''
if 'lines' in want:
    doc(os.path.join(out, 'lines.pdf'), [(3370, 2384, mk_lines)]); print('lines.pdf done', flush=True)
def mk_set(i):
    def f(w):
        tb = b'BT /F1 18 Tf 2100 60 Td (SHEET %d  -  SYNTHETIC DRAWING SET  -  SEE SHEET %d) Tj ET\n' % (i + 1, (i + 7) % 2000 + 1)
        return zlib.compress(lines(3000, 2384, 1684, rnd) + tb, 6), b''
    return f
if 'set' in want:
    doc(os.path.join(out, 'set.pdf'), [(2384, 1684, mk_set(i)) for i in range(2000)]); print('set.pdf done', flush=True)
IW, IH = 1400, 1250
def mk_big(w):
    im = w.obj(b'<< /Type /XObject /Subtype /Image /Width %d /Height %d /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length %d >>' % (IW, IH, IW * IH * 3), os.urandom(IW * IH * 3))
    return zlib.compress(b'q 1190 0 0 842 0 0 cm /Im0 Do Q\n'), b'/XObject << /Im0 %d 0 R >>' % im
if 'big' in want:
    doc(os.path.join(out, 'big.pdf'), [(1190, 842, mk_big) for _ in range(300)]); print('big.pdf done', flush=True)
# heavy.pdf: one A0 sheet with HEAVY_N lines, each its own stroked path (m l S), so the display-list
# read takes seconds (lines.pdf strokes 1000 segments per path and reads in ~0.3 s). Used by the worker's
# cancel test. Own random stream (seed 11), so lines/set stay byte-identical; streamed through zlib.
HEAVY_N = 10_000_000
def mk_heavy(w):
    r = random.Random(11); z = zlib.compressobj(1); raw = []; packed = []; pw, ph = 3370, 2384
    for i in range(HEAVY_N):
        if i % 1000 == 0: raw.append(b'%.2f w %.2f %.2f %.2f RG\n' % (r.choice((0.1, 0.25, 0.5)), r.random() * .5, r.random() * .5, r.random() * .5))
        x, y = r.random() * pw, r.random() * ph
        raw.append(b'%.1f %.1f m %.1f %.1f l S\n' % (x, y, x + r.uniform(-40, 40), y + r.uniform(-40, 40)))
        if len(raw) >= 100_000: packed.append(z.compress(b''.join(raw))); raw = []
    packed.append(z.compress(b''.join(raw))); packed.append(z.flush())
    return b''.join(packed), b''
if 'heavy' in want:
    doc(os.path.join(out, 'heavy.pdf'), [(3370, 2384, mk_heavy)]); print('heavy.pdf done', flush=True)
