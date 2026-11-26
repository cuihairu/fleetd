#!/usr/bin/env python3
"""Generate rotating wood-colored tray icon frames from the supplied gear SVG."""
import pathlib
import subprocess
import sys

ROOT = pathlib.Path('/home/cui/workspaces/fleetd/assets/tray')
SRC = ROOT / 'gear.svg'
OUT = ROOT / 'frames'
FRAMES = 24
SIZE = 64  # Windows tray icon native size
WOOD = '#C8A165'  # user choice 1: light oak

OUT.mkdir(exist_ok=True)
src = SRC.read_text(encoding='utf-8')

# Only change the fill color; keep every path datum exactly as supplied.
colored = src.replace('fill="#333333"', f'fill="{WOOD}"')
assert colored != src, 'fill color not found in SVG'
colored_svg = ROOT / 'gear-wood.svg'
colored_svg.write_text(colored, encoding='utf-8')

# rsvg-convert renders SVG; rotation via SVG transform on a wrapper group.
for i in range(FRAMES):
    deg = i * 360 / FRAMES
    rotated = colored.replace(
        '<path d=',
        f'<g transform="rotate({deg:.4f} 512 512)"><path d=',
        1,
    ).replace('p-id="4455"></path>', 'p-id="4455"></path></g>', 1)
    frame_svg = ROOT / f'_frame{i:02d}.svg'
    frame_png = OUT / f'gear-{i:02d}.png'
    frame_svg.write_text(rotated, encoding='utf-8')
    subprocess.run(
        ['rsvg-convert', '-w', str(SIZE), '-h', str(SIZE), '-o', str(frame_png), str(frame_svg)],
        check=True,
    )
    frame_svg.unlink()

# Verify count + all frames present
pngs = sorted(OUT.glob('gear-*.png'))
assert len(pngs) == FRAMES, f'expected {FRAMES} frames, got {len(pngs)}'
print(f'{len(pngs)} frames generated at {SIZE}x{SIZE}, wood={WOOD}')
print(f'first={pngs[0].name} last={pngs[-1].name} total_bytes={sum(p.stat().st_size for p in pngs)}')
