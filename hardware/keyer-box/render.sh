#!/bin/sh
# Renders the pictures in images/ and runs the fit checks.
# Needs OpenSCAD 2021.01 or newer; on a machine with no display, run it under
# xvfb-run (Linux). Usage: sh render.sh [extra openscad -D settings...]
set -e
cd "$(dirname "$0")"
OPENSCAD="${OPENSCAD:-openscad}"   # macOS: /Applications/OpenSCAD.app/Contents/MacOS/OpenSCAD
command -v "$OPENSCAD" >/dev/null 2>&1 || { echo "openscad not found: set OPENSCAD=/path/to/openscad"; exit 1; }
mkdir -p images
EXTRA="$*"   # e.g. -D board_l=71 (no spaces inside a setting)
OPTS="--imgsize=1600,1200 --colorscheme=Tomorrow"

pic() { # name part camera(centre x,y,z, rot x,y,z, distance) [ortho]
    proj=""
    [ "${4:-}" = ortho ] && proj="--projection=ortho"
    # shellcheck disable=SC2086
    "$OPENSCAD" -o "images/$1.png" $OPTS $proj --camera="$3" -D "part=\"$2\"" $EXTRA keyer-box.scad \
        >/dev/null 2>&1
}

# shellcheck disable=SC2086
"$OPENSCAD" -o /tmp/keyer-box-report.echo $EXTRA keyer-box.scad >/dev/null 2>&1
cat /tmp/keyer-box-report.echo
rm -f /tmp/keyer-box-report.echo

pic assembly      assembly        37,25,6,60,0,25,250
pic assembly-back assembly        37,25,6,60,0,205,250
pic open          open            37,25,0,50,0,20,230
pic exploded      exploded        37,25,22,70,0,30,290
pic section       section         37,25,5,90,0,0,200      ortho
pic section-corner section_corner 2,25,5,90,0,270,120     ortho
pic print-plate   print           37,-25,10,45,0,30,300
pic peg-test      peg_test        35,25,2,55,0,20,200
pic lid-top       lid             37,-25,0,180,0,0,190    ortho
pic lid-under     lid             37,-25,0,0,0,0,190      ortho

for c in check_lid_box check_board check_pico check_plug check_cable check_jacks; do
    # shellcheck disable=SC2086
    if "$OPENSCAD" -o /tmp/keyer-box-check.stl -D "part=\"$c\"" $EXTRA keyer-box.scad 2>&1 \
        | grep -q "top level object is empty"; then
        echo "$c: clear"
    else
        echo "$c: OVERLAP"
    fi
done
rm -f /tmp/keyer-box-check.stl
