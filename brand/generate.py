#!/usr/bin/env python3
"""Writes the Asagiri logo SVG family into brand/logo and brand/icon.

    python3 brand/generate.py brand && python3 brand/render.py brand

Asagiri is "morning mist". The mark is the sun coming up through it: a
disc, and three bars of mist drawn across and below it. render.py makes
the PNGs, the Windows .ico and the installer bitmaps from these SVGs.
"""
import os
import sys

OUT = sys.argv[1]  # brand dir

DAWN = "#F2684A"  # the sun
DAWN_HI = "#FFA36C"
INK = "#161B33"  # the sky before sunrise
INK_HI = "#232A4D"
CHALK = "#F3F1F7"  # mist

# ---------- mark (64 x 64 design grid) ----------
# The disc shows above the first bar and once more between the first and
# the second; the gaps beside each bar are two units, so the mark holds
# together in one colour.
SUN = '<circle cx="32" cy="33" r="20"/>'
SHOW = '<rect x="0" y="0" width="64" height="29.5"/><rect x="0" y="39" width="64" height="4"/>'
BARS = ('<rect x="3" y="31.5" width="41" height="5.5" rx="2.75"/>'
        '<rect x="19" y="45" width="42" height="5.5" rx="2.75"/>'
        '<rect x="9" y="54.5" width="30" height="5.5" rx="2.75"/>')


def mark(sun, bars, uid):
    return f'''<clipPath id="{uid}-c">{SHOW}</clipPath>
  <g clip-path="url(#{uid}-c)" fill="{sun}">{SUN}</g>
  <g fill="{bars}">{BARS}</g>'''


# ---------- wordmark (geometric strokes, x-height 30.5..51.5) ----------
# Round caps, one stroke weight. The dots of the i's are two small suns.
SW = 6.6
WM_W = 178  # wordmark width in its own units
WM_H = 68  # down to the tail of the g


def wordmark(ink, dot):
    def a(cx):
        return f'<circle cx="{cx}" cy="41" r="10.5"/><path d="M{cx + 10.5} 30.5V51.5"/>'

    def s(x):
        return (f'<path d="M{x + 14.5} 35C{x + 12} 31.5 {x + 9} 30.5 {x + 7.5} 30.5C{x + 3} 30.5 {x + 1} 33 {x + 1} 35.8'
                f'C{x + 1} 39.5 {x + 4} 40.4 {x + 7.8} 41C{x + 11.5} 41.6 {x + 14.8} 42.6 {x + 14.8} 46.2'
                f'C{x + 14.8} 49.2 {x + 12.2} 51.5 {x + 7.8} 51.5C{x + 4.8} 51.5 {x + 2.2} 50.4 {x + 0.3} 47.6"/>')

    def g(cx):
        return f'<circle cx="{cx}" cy="41" r="10.5"/><path d="M{cx + 10.5} 30.5V54.5C{cx + 10.5} 62.5 {cx + 2} 65 {cx - 7} 61"/>'

    def i(x):
        return f'<path d="M{x} 30.5V51.5"/>'

    def r(x):
        return f'<path d="M{x} 30.5V51.5"/><path d="M{x} 41C{x} 34 {x + 5} 30.5 {x + 12.5} 30.5"/>'

    body = a(15) + s(38.5) + a(76.5) + g(110.5) + i(134) + r(147) + i(172.5)
    dots = '<circle cx="134" cy="16.5" r="4.6"/><circle cx="172.5" cy="16.5" r="4.6"/>'
    return (f'<g fill="none" stroke="{ink}" stroke-width="{SW}" stroke-linecap="round" stroke-linejoin="round">{body}</g>\n'
            f'  <g fill="{dot}">{dots}</g>')


def svg(w, h, body, title):
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w:g} {h:g}" width="{w * 4:g}" height="{h * 4:g}" '
            f'role="img" aria-labelledby="t">\n  <title id="t">{title}</title>\n  {body}\n</svg>\n')


def write(path, text):
    full = os.path.join(OUT, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, 'w') as f:
        f.write(text)
    print('wrote', full)


variants = {
    'light': dict(sun=DAWN, bars=INK, ink=INK, dot=DAWN),
    'dark': dict(sun=DAWN, bars=CHALK, ink=CHALK, dot=DAWN),
    'mono': dict(sun=INK, bars=INK, ink=INK, dot=INK),
    'mono-white': dict(sun=CHALK, bars=CHALK, ink=CHALK, dot=CHALK),
}

for name, v in variants.items():
    uid = f'a{name.replace("-", "")}'
    m = mark(v['sun'], v['bars'], uid)
    wm = wordmark(v['ink'], v['dot'])
    # mark only
    write(f'logo/asagiri-mark-{name}.svg', svg(64, 64, m, 'Asagiri'))
    # horizontal: the wordmark's x-height sits between the first bar and the last
    scale = 0.7
    body = f'{m}\n  <g transform="translate(80 5) scale({scale})">{wm}</g>'
    write(f'logo/asagiri-horizontal-{name}.svg', svg(80 + WM_W * scale + 2, 64, body, 'Asagiri'))
    # stacked: the mark above, the wordmark centred below
    sw = WM_W + 12
    ms = 1.9
    body = (f'<g transform="translate({(sw - 64 * ms) / 2:g} 0) scale({ms})">{m}</g>\n'
            f'  <g transform="translate({(sw - WM_W) / 2:g} {64 * ms + 2:g})">{wm}</g>')
    write(f'logo/asagiri-stacked-{name}.svg', svg(sw, 64 * ms + 2 + WM_H, body, 'Asagiri'))

# ---------- favicon: the sun and two bars on a night tile ----------
fav = f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32" width="32" height="32">
  <title>Asagiri</title>
  <rect width="32" height="32" rx="7" fill="{INK}"/>
  <clipPath id="fc"><rect x="0" y="0" width="32" height="15.5"/></clipPath>
  <circle cx="16" cy="16.5" r="10" fill="{DAWN}" clip-path="url(#fc)"/>
  <rect x="3.5" y="17.5" width="20" height="3.6" rx="1.8" fill="{CHALK}"/>
  <rect x="9.5" y="23.5" width="19" height="3.6" rx="1.8" fill="{CHALK}"/>
</svg>
'''
write('logo/asagiri-favicon.svg', fav)


# ---------- app icon 1024 ----------
def app_icon(m, rx, width, tile_note):
    """The app icon: a rounded tile `m` px in from each edge, and the mark
    drawn `width` of the tile wide, centred. The drawing spans x 3..61 and
    y 13..60 of its 64-unit grid, so it is centred on (32, 36.5)."""
    t = 1024 - 2 * m
    scale = round(width * t / 58, 3)
    ax, ay = round(512 - 32 * scale, 1), round(512 - 36.5 * scale, 1)
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" width="1024" height="1024">
  <title>Asagiri app icon</title>
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{INK_HI}"/>
      <stop offset="1" stop-color="{INK}"/>
    </linearGradient>
    <radialGradient id="glow" cx="0.5" cy="0.42" r="0.55">
      <stop offset="0" stop-color="{DAWN}" stop-opacity="0.34"/>
      <stop offset="1" stop-color="{DAWN}" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="sun" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{DAWN_HI}"/>
      <stop offset="1" stop-color="{DAWN}"/>
    </linearGradient>
  </defs>
  <!-- {tile_note} -->
  <rect x="{m}" y="{m}" width="{t}" height="{t}" rx="{rx}" fill="url(#bg)"/>
  <rect x="{m}" y="{m}" width="{t}" height="{t}" rx="{rx}" fill="url(#glow)"/>
  <rect x="{m + 1}" y="{m + 1}" width="{t - 2}" height="{t - 2}" rx="{rx - 1}" fill="none" stroke="#FFFFFF" stroke-opacity="0.07" stroke-width="2"/>
  <!-- mark: {width:.0%} of the tile wide, centred -->
  <g transform="translate({ax:g} {ay:g}) scale({scale:g})">
    <clipPath id="ic">{SHOW}</clipPath>
    <g clip-path="url(#ic)" fill="url(#sun)">{SUN}</g>
    <g fill="{CHALK}">{BARS}</g>
  </g>
</svg>
'''


write('icon/asagiri-app-icon.svg', app_icon(100, 185, 0.7, 'macOS-style rounded tile with 100px safe margin'))
# Windows draws icons to the edge of their square, and Apple's margin makes
# an icon look a size small in the taskbar: the tile fills the canvas (same
# corner ratio) and the mark is drawn larger. render.py makes the PNGs and
# the .ico from it.
write('icon/asagiri-app-icon-windows.svg', app_icon(0, 230, 0.74, 'Windows: the tile fills the square, the mark drawn larger'))
