#!/usr/bin/env python3
"""Render docs/algorithm.gif: one real Blink search, one frame per provider request.

Usage: python3 docs/algorithm-animation.py RECEIPT.json ROOT QUERY OUT_DIR
  RECEIPT.json  output of `examples/evaluate_case` for the search (it carries judgment_events)
  ROOT          the searched directory (to cut the same windows Blink cut)
  QUERY         the question, for the caption
  OUT_DIR       where frames go; assemble with
    ffmpeg -framerate 5 -i OUT_DIR/f%04d.png -vf "fps=5,split[a][b];[a]palettegen=max_colors=64[p];[b][p]paletteuse=dither=none" docs/algorithm.gif
Needs Pillow and a built `target/release/blink` for the file inventory.
"""
import json, subprocess, sys
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont

def windows_for(files):
    """Mirror of Blink's window cut: 80 lines or 4 KiB, eight overlapping lines, UTF-8 safe."""
    windows = []
    for fi, (path, text) in enumerate(files):
        data = text.encode()
        lines = [0] + [i + 1 for i, b in enumerate(data) if b == 10]
        start = 0
        while start < len(data):
            start_line = sum(1 for o in lines if o <= start)
            line_end = lines[start_line - 1 + 80] if start_line - 1 + 80 < len(lines) else len(data)
            end = min(line_end, start + 4096, len(data))
            while end < len(data) and (data[end] & 0xC0) == 0x80:
                end -= 1
            end_line = sum(1 for o in lines if o < end)
            windows.append(dict(file=fi, path=path, start=start, end=end, start_line=start_line, end_line=end_line))
            if end == len(data):
                break
            overlap_line = max(end_line - 8, start_line)
            overlap = lines[overlap_line] if overlap_line < len(lines) else end
            start = overlap if start < overlap < end else end
    return windows

EVENTS, ROOT, QUERY, OUT = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3], Path(sys.argv[4])
BLINK = str(Path(__file__).resolve().parent.parent / 'target/release/blink')
OUT.mkdir(parents=True, exist_ok=True)
W, H = 1000, 560
BG = (17, 19, 24); PANEL = (24, 27, 34); TEXT = (220, 224, 232); DIM = (120, 126, 140)
UNREAD = (44, 48, 60); REJECT = (72, 78, 96); ACCEPT = (61, 220, 132); NOMINATE = (255, 184, 48); EVIDENCE = (167, 139, 250); FLASH = (255, 255, 255); RETRACT = (235, 87, 87)
FONT = '/System/Library/Fonts/Menlo.ttc'
f_small = ImageFont.truetype(FONT, 13); f_med = ImageFont.truetype(FONT, 16); f_big = ImageFont.truetype(FONT, 20)

d = json.loads(EVENTS.read_text())
inv = json.loads(subprocess.run([BLINK, 'files', str(ROOT), '--json'], check=True, capture_output=True).stdout)
windows = windows_for([(f['path'], (ROOT / f['path']).read_bytes().decode()) for f in inv['files']])
by_name = {f"w{i}": i for i in range(len(windows))}
files = sorted({w['path'] for w in windows})
file_windows = {p: [i for i, w in enumerate(windows) if w['path'] == p] for p in files}

# Layout: one column per file, blocks stacked top-down, grouped by directory with gaps.
col_w, block_h, gap = 10, 10, 1
x0, y0 = 24, 92
cols = {}
x = x0; last_dir = None
for p in files:
    dirname = p.rsplit('/', 1)[0] if '/' in p else ''
    if last_dir is not None and dirname != last_dir:
        x += 10
    cols[p] = x; x += col_w + gap; last_dir = dirname
grid_w = x - x0
scale = min(1.0, 570 / grid_w)

def block_rect(index):
    w = windows[index]; p = w['path']; row = file_windows[p].index(index)
    bx = x0 + (cols[p] - x0) * scale; return (bx, y0 + row * (block_h + gap), bx + max(2, col_w * scale) - 1, y0 + row * (block_h + gap) + block_h - 1)

# Group events into requests: consecutive events with the same donor kind form a batch of at most 8 (4 for evidence).
events = d['judgment_events']
batches = []; cur = []
def kind(e): return 'evidence' if e['donor'] == 'evidence' else ('related' if e['donor'] else 'plain')
for e in events:
    if cur and (kind(e) != kind(cur[0]) or (kind(e) == 'related' and e['donor'] != cur[0]['donor']) or len(cur) >= (4 if kind(e) == 'evidence' else 8)):
        batches.append(cur); cur = []
    cur.append(e)
if cur: batches.append(cur)
first_related = next((i for i, b in enumerate(batches) if kind(b[0]) == 'related'), len(batches))
def phase_of(i, b):
    k = kind(b[0])
    if k == 'plain' and i < first_related: return ('Pass 1', 'read the likeliest windows,', '8 per request + listwise question')
    if k == 'related': return ('Pass 2', 'follow shared identifiers out', 'of accepted code, caller attached')
    if k == 'evidence': return ('Pass 3', 'second look at near misses,', 'accepted excerpts as evidence')
    return ('Pass 4', 'read deeper into files that', 'already hold accepted code')

state = {}  # index -> (score, choice, kind)
def draw_frame(step, flash, label, done=False, shown_paths=()):
    img = Image.new('RGB', (W, H), BG); dr = ImageDraw.Draw(img)
    dr.text((24, 18), 'blink search', font=f_big, fill=TEXT)
    dr.text((24, 46), '"' + QUERY + '"', font=f_med, fill=DIM)
    dr.text((24, 70), f'{"/".join(ROOT.parts[-2:])}: {len(files)} files as columns, {len(windows)} windows of up to 80 lines as blocks', font=f_small, fill=DIM)
    for i in range(len(windows)):
        s = state.get(i); color = UNREAD
        if s:
            score, choice, k = s
            if score is None: color = REJECT
            elif score >= 0.5: color = ACCEPT
            elif choice is not None and choice >= 0.6: color = NOMINATE
            else: color = REJECT
            if s[2] == 'evidence' and score is not None and score < 0.5: color = EVIDENCE
        if i in flash: color = FLASH
        dr.rectangle(block_rect(i), fill=color)
    # right panel
    px = 620
    dr.rectangle((px - 10, 92, W - 24, H - 24), fill=PANEL)
    if isinstance(label, tuple):
        dr.text((px, 104), label[0], font=f_med, fill=ACCEPT); dr.text((px + 80, 106), label[1], font=f_small, fill=TEXT); dr.text((px + 80, 122), label[2], font=f_small, fill=TEXT)
    else:
        dr.text((px, 104), label, font=f_small, fill=TEXT)
    total = len(batches) + 2
    dr.rectangle((px, 148, px + 330, 152), fill=UNREAD); dr.rectangle((px, 148, px + int(330 * min(1.0, step / max(1, total))), 152), fill=ACCEPT)
    judged = len(state); accepted = sum(1 for s in state.values() if s[0] is not None and s[0] >= 0.5)
    nominated = sum(1 for s in state.values() if s[0] is not None and s[0] < 0.5 and s[1] is not None and s[1] >= 0.6)
    lines = [f'requests      {step}', f'windows judged {judged} / {len(windows)}', f'accepted      {accepted}', f'nominated     {nominated}', '']
    for n, t in enumerate(lines): dr.text((px, 162 + n * 20), t, font=f_small, fill=TEXT)
    legend = [(ACCEPT, 'accepted (p >= 0.5)'), (NOMINATE, 'nominated by the listwise question'), (EVIDENCE, 'rejected with evidence attached'), (REJECT, 'judged, rejected'), (UNREAD, 'not read')]
    for n, (c, t) in enumerate(legend):
        y = 262 + n * 20; dr.rectangle((px, y + 3, px + 10, y + 13), fill=c); dr.text((px + 18, y), t, font=f_small, fill=DIM)
    if done:
        dr.text((px, 370), f"done in {d['budgets']['elapsed_ms']} ms, {d['budgets']['attempts']} requests", font=f_med, fill=ACCEPT)
        dr.text((px, 396), 'returned, exact bytes with sha256:', font=f_small, fill=TEXT)
        for n, r in enumerate(d['results'][:5]):
            dr.text((px, 418 + n * 20), f"{r['path']}:{r['start_line']}-{r['end_line']}", font=f_small, fill=TEXT)
    else:
        # show the batch's paths
        shown = list(shown_paths)[:6]
        dr.text((px, 370), 'this request judges:', font=f_small, fill=TEXT)
        for n, p in enumerate(shown): dr.text((px, 392 + n * 18), p, font=f_small, fill=DIM)
    return img

frames = []
frames.append(draw_frame(0, set(), 'plan: list eligible files, cut 80-line windows, rank by query terms'))
frames.append(frames[0])
frames.append(draw_frame(2, set(), 'preview: two requests score short cards for every region'))
frames.append(frames[-1])
for i, b in enumerate(batches):
    flash = {by_name[e['name']] for e in b if e['name'] in by_name}
    label = phase_of(i, b)
    paths = sorted({windows[by_name[e['name']]]['path'] for e in b if e['name'] in by_name})
    frames.append(draw_frame(i + 3, flash, label, shown_paths=paths))
    for e in b:
        if e['name'] in by_name:
            idx = by_name[e['name']]
            prev = state.get(idx)
            new_kind = kind(e)
            state[idx] = (e['score'], e.get('choice'), new_kind)
    frames.append(draw_frame(i + 3, set(), label, shown_paths=paths))
final = draw_frame(len(batches) + 2, set(), 'finished: results rechecked against file hashes', done=True)
for _ in range(15): frames.append(final)
for n, fr in enumerate(frames): fr.save(OUT / f'f{n:04d}.png')
print('frames', len(frames), 'batches', len(batches))
