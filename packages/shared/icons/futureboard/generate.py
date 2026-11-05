# Writes the Futureboard icon set: original 16x16 drawings, authored here
# coordinate by coordinate. Usage: python generate.py <output dir>
# (normally this directory: python generate.py .)
import os
import sys

out = sys.argv[1]
os.makedirs(out, exist_ok=True)

# Shared vocabulary. Masses are solid fills softened by a 1 px round-join
# stroke of the same colour (a 0.5 px corner); linear forms are 1.75 px with
# round caps and joins; detail is cut out of a mass with a mask at 1.25 px.
SOFT = 'fill="currentColor" stroke="currentColor" stroke-width="1" stroke-linejoin="round"'
FILL = 'fill="currentColor"'
LINE = 'fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"'


def svg(body):
    return (
        '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" '
        'xmlns="http://www.w3.org/2000/svg">' + body + "</svg>\n"
    )


def knockout(mask_id, cuts, shape):
    """`shape` with `cuts` (paths drawn in black, 1.25 px) removed."""
    return (
        f'<mask id="{mask_id}" maskUnits="userSpaceOnUse" x="0" y="0" width="16" height="16">'
        '<rect width="16" height="16" fill="#fff"/>'
        f'<g fill="none" stroke="#000" stroke-width="1.25" stroke-linecap="round">{cuts}</g>'
        f'</mask><g mask="url(#{mask_id})">{shape}</g>'
    )


icons = {
    # ── Transport ──────────────────────────────────────────────────────────
    "play": f'<path d="M4.5 2.75 L12.75 8 L4.5 13.25 Z" {SOFT}/>',
    "pause": f'<rect x="3.5" y="2.75" width="3.25" height="10.5" rx="1" {FILL}/>'
    f'<rect x="9.25" y="2.75" width="3.25" height="10.5" rx="1" {FILL}/>',
    "stop": f'<rect x="3" y="3" width="10" height="10" rx="1.5" {FILL}/>',
    "record": f'<circle cx="8" cy="8" r="5.25" {FILL}/>',
    "skip-back": f'<rect x="2.5" y="3" width="2" height="10" rx="0.75" {FILL}/>'
    f'<path d="M13.25 3.25 L6 8 L13.25 12.75 Z" {SOFT}/>',
    "cycle": f'<path d="M2.75 9.25 V6.5 A2 2 0 0 1 4.75 4.5 H10.5" {LINE}/>'
    f'<path d="M10 1.75 L13.5 4.5 L10 7.25 Z" {SOFT}/>'
    f'<path d="M13.25 6.75 V9.5 A2 2 0 0 1 11.25 11.5 H5.5" {LINE}/>'
    f'<path d="M6 8.75 L2.5 11.5 L6 14.25 Z" {SOFT}/>',
    "metronome": f'<path d="M3 11.25 H13 L13.75 14 H2.25 Z" {SOFT}/>'
    f'<path d="M3.75 11 L6.25 2.5 H9.75 L12.25 11" {LINE}/>'
    f'<path d="M8 11 L11.25 4.75" {LINE}/>'
    f'<circle cx="10" cy="7.15" r="1.35" {FILL}/>',
    # ── Edit tools ─────────────────────────────────────────────────────────
    "pointer": f'<path d="M3.5 2 L12.25 9.1 L8.6 9.5 L10.75 13.4 L9.05 14.35 L6.9 10.45 L4.4 13 Z" {SOFT}/>',
    "pencil": knockout(
        "k",
        '<path d="M8.9 3.7 L12.3 7.1"/><path d="M3.15 9.75 L6.25 12.85"/>',
        f'<path d="M2.25 13.75 L3.05 10.15 L10.4 2.8 A1.5 1.5 0 0 1 12.5 2.8 L13.2 3.5 '
        f'A1.5 1.5 0 0 1 13.2 5.6 L5.85 12.95 Z" {SOFT}/>',
    ),
    "scissors": f'<path d="M6.4 9.6 L12.9 2.4" {LINE}/>'
    f'<path d="M9.6 9.6 L3.1 2.4" {LINE}/>'
    f'<path fill-rule="evenodd" clip-rule="evenodd" {FILL} d="M4.6 8.85 A2.65 2.65 0 1 1 4.6 14.15 '
    f'A2.65 2.65 0 1 1 4.6 8.85 Z M4.6 10.3 A1.2 1.2 0 1 0 4.6 12.7 A1.2 1.2 0 1 0 4.6 10.3 Z"/>'
    f'<path fill-rule="evenodd" clip-rule="evenodd" {FILL} d="M11.4 8.85 A2.65 2.65 0 1 1 11.4 14.15 '
    f'A2.65 2.65 0 1 1 11.4 8.85 Z M11.4 10.3 A1.2 1.2 0 1 0 11.4 12.7 A1.2 1.2 0 1 0 11.4 10.3 Z"/>',
    "eraser": knockout(
        "k",
        '<path d="M5.6 5.4 L10.6 10.4"/>',
        f'<rect x="2.5" y="5" width="11" height="6" rx="1.5" transform="rotate(-45 8 8)" {FILL}/>',
    )
    + f'<path d="M9 14.25 H14" {LINE}/>',
    "zoom-in": f'<circle cx="7" cy="7" r="4.4" {LINE}/>'
    f'<path d="M10.3 10.3 L13.9 13.9" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"/>'
    f'<path d="M5 7 H9 M7 5 V9" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>',
    "zoom-out": f'<circle cx="7" cy="7" r="4.4" {LINE}/>'
    f'<path d="M10.3 10.3 L13.9 13.9" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"/>'
    f'<path d="M5 7 H9" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>',
    "snap": knockout(
        "k",
        '<path d="M2 5.75 H5.6 M10.4 5.75 H14"/>',
        '<path d="M4 2 V8.25 A4 4 0 0 0 12 8.25 V2" fill="none" stroke="currentColor" stroke-width="3"/>',
    ),
    # ── Track and signal types ─────────────────────────────────────────────
    "mic": knockout(
        "k",
        '<path d="M6.9 4.6 H9.1 M6.9 6.9 H9.1"/>',
        f'<rect x="5.5" y="1.5" width="5" height="8.25" rx="2.5" {FILL}/>',
    )
    + f'<path d="M3.5 7.75 A4.5 4.5 0 0 0 12.5 7.75 M8 12.25 V14.25 M5.75 14.25 H10.25" {LINE}/>',
    "instrument": f'<rect x="1.75" y="3.25" width="12.5" height="9.5" rx="1.25" '
    f'fill="none" stroke="currentColor" stroke-width="1.5"/>'
    f'<rect x="3.8" y="3.25" width="2.15" height="5.5" rx="0.5" {FILL}/>'
    f'<rect x="6.95" y="3.25" width="2.15" height="5.5" rx="0.5" {FILL}/>'
    f'<path d="M4.875 8.75 V12.75 M8 8.75 V12.75 M11.125 3.25 V12.75" '
    f'fill="none" stroke="currentColor" stroke-width="1.25"/>',
    "midi-note": f'<ellipse cx="5.5" cy="11.75" rx="3" ry="2.2" transform="rotate(-20 5.5 11.75)" {FILL}/>'
    f'<rect x="7.4" y="1.75" width="1.6" height="10.25" rx="0.5" {FILL}/>'
    f'<path d="M9 1.75 C11.4 2.6 13.4 4.4 12.9 7.9 C12.3 6.4 10.9 5.6 9 5.4 Z" {FILL}/>',
    "waveform": "".join(
        f'<rect x="{x}" y="{8 - h / 2}" width="1.6" height="{h}" rx="0.8" {FILL}/>'
        for x, h in [(1.6, 4), (4.3, 10), (7.2, 13), (10.1, 7.5), (12.8, 4.5)]
    ),
    "speaker": f'<path d="M2 6 H4.75 L8.75 2.75 V13.25 L4.75 10 H2 Z" {SOFT}/>'
    f'<path d="M11 5.6 A3.4 3.4 0 0 1 11 10.4 M12.9 3.7 A6 6 0 0 1 12.9 12.3" {LINE}/>',
    "mute": f'<path d="M2 6 H4.75 L8.75 2.75 V13.25 L4.75 10 H2 Z" {SOFT}/>'
    f'<path d="M11 6 L14.25 9.25 M14.25 6 L11 9.25" {LINE}/>',
    "bus": f'<path d="M3.25 3.25 L8 8 L3.25 12.75 M8 8 H12" {LINE}/>'
    f'<circle cx="3.25" cy="3.25" r="1.6" {FILL}/>'
    f'<circle cx="3.25" cy="12.75" r="1.6" {FILL}/>'
    f'<path d="M11.25 5.25 L14.5 8 L11.25 10.75 Z" {SOFT}/>',
    "group": f'<rect x="1.75" y="2.25" width="6.5" height="2.75" rx="0.75" {FILL}/>'
    f'<rect x="1.75" y="6.625" width="6.5" height="2.75" rx="0.75" {FILL}/>'
    f'<rect x="1.75" y="11" width="6.5" height="2.75" rx="0.75" {FILL}/>'
    f'<path d="M9.75 3.625 H11.25 V12.375 H9.75 M11.25 8 H14.25" {LINE}/>',
    "return": f'<path d="M13.25 2.75 V8 A2 2 0 0 1 11.25 10 H5" {LINE}/>'
    f'<path d="M6.25 6.75 L2.25 10 L6.25 13.25 Z" {SOFT}/>',
    "video": knockout(
        "k",
        "".join(
            f'<path d="M{x} {y} H{x + 0.01}"/>'
            for x in (3.2, 12.8)
            for y in (4.6, 8, 11.4)
        )
        + '<rect x="5.6" y="4.4" width="4.8" height="7.2" rx="0.6" fill="#000" stroke="none"/>',
        f'<rect x="1.5" y="2.5" width="13" height="11" rx="1.5" {FILL}/>',
    ),
    # ── Mixer, routing, processing ─────────────────────────────────────────
    "mixer": f'<path d="M3.5 2 V14 M8 2 V14 M12.5 2 V14" fill="none" stroke="currentColor" '
    f'stroke-width="1.25" stroke-linecap="round"/>'
    f'<rect x="1.75" y="8.5" width="3.5" height="2.75" rx="0.75" {FILL}/>'
    f'<rect x="6.25" y="3.75" width="3.5" height="2.75" rx="0.75" {FILL}/>'
    f'<rect x="10.75" y="6.75" width="3.5" height="2.75" rx="0.75" {FILL}/>',
    "automation": f'<path d="M2.25 12 L6 5.25 L10 9.25 L13.75 3.25" {LINE}/>'
    f'<rect x="4.4" y="3.65" width="3.2" height="3.2" rx="0.8" {FILL}/>'
    f'<rect x="8.4" y="7.65" width="3.2" height="3.2" rx="0.8" {FILL}/>'
    f'<circle cx="2.25" cy="12" r="1.5" {FILL}/>'
    f'<circle cx="13.75" cy="3.25" r="1.5" {FILL}/>',
    "insert": f'<rect x="5.25" y="1.75" width="1.5" height="4" rx="0.5" {FILL}/>'
    f'<rect x="9.25" y="1.75" width="1.5" height="4" rx="0.5" {FILL}/>'
    f'<path d="M3.75 5.75 H12.25 V8.25 A4.25 4.25 0 0 1 3.75 8.25 Z" {SOFT}/>'
    f'<path d="M8 12.5 V14.5" {LINE}/>',
    "routing": f'<circle cx="3.75" cy="4" r="2" {FILL}/>'
    f'<circle cx="12.25" cy="12" r="2" {FILL}/>'
    f'<path d="M3.75 6 C3.75 11.25 12.25 4.75 12.25 10" {LINE}/>',
    "freeze": "".join(
        f'<g transform="rotate({angle} 8 8)"><path d="M8 1.75 V14.25 M6.25 3.25 L8 5 L9.75 3.25 '
        f'M6.25 12.75 L8 11 L9.75 12.75" fill="none" stroke="currentColor" stroke-width="1.5" '
        f'stroke-linecap="round" stroke-linejoin="round"/></g>'
        for angle in (0, 60, 120)
    ),
    "power": f'<path d="M5 4.4 A5 5 0 1 0 11 4.4" {LINE}/><path d="M8 1.75 V7.75" {LINE}/>',
    "mono": f'<circle cx="8" cy="8" r="5.5" fill="none" stroke="currentColor" stroke-width="1.75"/>'
    f'<circle cx="8" cy="8" r="2.4" {FILL}/>',
    "stack": f'<path d="M8 1.75 L14.25 4.75 L8 7.75 L1.75 4.75 Z" {SOFT}/>'
    f'<path d="M1.75 8 L8 11 L14.25 8" {LINE}/>'
    f'<path d="M1.75 11.25 L8 14.25 L14.25 11.25" {LINE}/>',
}

for name, body in icons.items():
    with open(os.path.join(out, f"{name}.svg"), "w", encoding="utf-8", newline="\n") as f:
        f.write(svg(body))
print(len(icons), "icons written to", out)
