// Keyer box enclosure: a printed box and snap-on lid for the 70 x 50 mm
// perfboard that carries the Pico 2 keyer (firmware/pico2-keyer, docs/keyer.md).
//
// OpenSCAD 2021.01 or newer. Open this file, then Window > Customizer to change
// the numbers below with sliders and menus. To export a part: set "part" to box
// (or lid), press F6 (fn+F6 on a Mac, or Design > Render), then
// File > Export > Export as STL. Both parts come out already turned the way they
// go on the print plate. No supports needed.
//
// Everything is measured from the perfboard: x along its 70 mm side, y along its
// 50 mm side, origin at its front left corner, z = 0 on its underside. "Left",
// "right", "front" and "back" are the box walls as seen from the front.
//
// Nothing here changes the keyer's wiring. The box only holds the board.

/* [What to show] */
// box and lid are the parts to export; peg_test is a quick print to try your board on the pegs; print lays both parts out as on the plate; check_* render to nothing when the parts clear each other; the rest are pictures
part = "assembly"; // [assembly, open, exploded, section, section_corner, box, lid, print, peg_test, check_lid_box, check_board, check_pico, check_plug, check_cable, check_jacks]
// Draw the perfboard, the Pico 2, the USB plug, the key lead and jack bodies in the pictures
show_parts = true;

/* [Perfboard (check with calipers)] */
// Board length (left to right)
board_l = 70;     // [40:0.1:150]
// Board width (front to back)
board_w = 50;     // [30:0.1:120]
// Board thickness
board_t = 1.6;    // [0.8:0.1:2.4]
// Corner mounting hole diameter
hole_d = 2.0;     // [1.0:0.1:4.0]
// Corner hole centre, distance from the left and right edges
hole_inset_x = 2.0; // [1.0:0.1:10]
// Corner hole centre, distance from the front and back edges
hole_inset_y = 2.0; // [1.0:0.1:10]
// Hole grid (24 x 18 on the 7 x 5 cm board, centred on it)
grid_cols = 24;   // [1:1:60]
grid_rows = 18;   // [1:1:60]
pitch = 2.54;

/* [Box] */
wall = 2.0;       // [1.2:0.1:4]
floor_t = 2.0;    // [1.2:0.1:4]
lid_t = 2.0;      // [1.2:0.1:4]
// Gap between the board's edge and the inside of the walls, each side
board_gap = 0.6;  // [0.2:0.1:3]
// Room under the board for solder joints and wires
below = 5.0;      // [2:0.5:20]
// Room over the board, from its top to the underside of the lid
above = 14.0;     // [6:0.5:40]
// Outside corner radius
corner_r = 3.0;   // [0.5:0.5:8]
// Sliding clearance between lid and box, each side (PETG on a Bambu: 0.15 to 0.25)
fit = 0.2;        // [0.05:0.05:0.5]

/* [Holding the board] */
// pins: printed pegs through the corner holes, the lid's posts stop the board lifting off them (no hardware). screws: posts with pilot holes for M2 self-tapping screws
mount = "pins";   // [pins, screws]
// Peg diameter = hole diameter minus this
pin_clear = 0.4;  // [0.1:0.05:0.8]
// How far the pegs stick up through the board
pin_above = 1.0;  // [0.5:0.1:3]
// Gap between the lid's posts and the top of the board (they stop the board lifting off its pegs)
press_gap = 0.2;  // [0:0.05:1]
// Diameter of the posts under and over the board's corners
post_d = 4.6;     // [3:0.1:8]
// Lid post diameter
press_d = 4.4;    // [3:0.1:8]
screw_pilot_d = 1.6; // [1:0.1:3]
// Small ribs on the walls beside the board's edges, so that pushing in the USB plug loads the walls, not the pegs
edge_stops = true;
// Gap between those ribs and the board's edge
edge_stop_gap = 0.2; // [0.05:0.05:1]

/* [Lid] */
// Snap ridges on the lid's skirt that click into grooves in the long walls
snap = true;
skirt_h = 4.0;    // [2:0.5:8]
skirt_t = 1.6;    // [1:0.1:3]
// Snap ridge height
bump = 0.6;       // [0.3:0.05:1.0]
bump_len = 20;    // [5:1:50]
// Depth of the snap ridge below the rim
bump_drop = 2.0;  // [1:0.1:3]
// Fingernail notch in the front rim for opening the lid
pry_notch = true;
// Big text on the lid
lid_label = "CW KEYER";
// Smaller line under it (your callsign)
lid_label2 = "K8ONS";
lid_label_size = 6; // [3:0.5:12]
// Window over the Pico 2's LED (lit while the key is closed, flashing after a trip)
led_window = true;
led_window_d = 6; // [2:0.5:10]
led_label = "TX";
// Hole over the BOOTSEL button (off: take the lid off to re-flash)
bootsel_hole = false;
bootsel_d = 4.5;  // [2:0.5:8]
// Text cut into the lid and walls
text_depth = 0.6; // [0:0.1:1.2]
wall_labels = true;
wall_label_size = 4.5; // [2:0.5:8]

/* [Pico 2 position on the board] */
// The wall the Pico 2's USB socket faces (on a 70 x 50 board only left or right fit; the openings below do not move with it)
pico_usb_wall = "left"; // [left, right, front, back]
// Distance from that board edge to the Pico's USB end; -1 = pin 1 in the hole column given next
pico_inset = -1;  // [-1:0.01:40]
// Used when pico_inset is -1: hole column for pin 1, counted from that edge (0 = the first column)
pico_inset_holes = 0; // [0:1:20]
// Centre line of the Pico along that wall (mm from the board's left or front edge); -1 = centre of the board
pico_center = -1; // [-1:0.01:150]
// Gap from the top of the perfboard to the underside of the Pico: 2.5 on pin headers; 8.5 in sockets (then set above to 17.5 or more); 0 soldered flat (then put its USB end at the board's edge)
pico_height = 2.5; // [0:0.1:12]
// LED position on the Pico: mm in from the USB end, and mm from the centre line toward pin 1 (estimate)
pico_led_p = 3.5; // [0:0.1:51]
pico_led_q = 5.0; // [-10:0.1:10]
// BOOTSEL button position, the same way (estimate: check yours)
pico_bootsel_p = 12.0; // [0:0.1:51]
pico_bootsel_q = 3.5;  // [-10:0.1:10]

/* [USB opening] */
// Big enough for the plug's moulded body to go into the wall (yours looked about 15 mm wide)
usb_hole_w = 16.5; // [8:0.5:25]
usb_hole_h = 10;   // [4:0.5:15]
usb_hole_r = 2;    // [0:0.5:5]
// Moulded body of your USB plug: width, thickness, length (measure yours; used for the pictures and the fit check)
usb_plug_w = 15;   // [8:0.5:25]
usb_plug_h = 8;    // [4:0.5:15]
usb_plug_len = 18; // [5:1:40]

/* [Opening 1: key lead] */
o1_on = true;
// Name cut into the wall under it
o1_name = "KEY";
o1_wall = "right"; // [left, right, front, back]
// cable: slot open to the rim, the lid's tongue closes over the cable. round: a hole for the cable (a panel jack on the key line needs the safety audit first). rect: rounded rectangle
o1_shape = "cable"; // [cable, round, rect]
// Centre along the wall, mm from the board's left (front/back walls) or front (left/right walls) edge
o1_along = 25;    // [0:0.5:150]
// Centre height above the top of the board
o1_z = 4;         // [-10:0.5:40]
// Width or diameter (for cable: the cable's diameter)
o1_w = 4.0;       // [1:0.1:30]
// Height (rect only)
o1_h = 4.0;       // [1:0.1:30]
// How much the lid's tongue squeezes the cable (cable shape only)
cable_pinch = 0.4; // [0:0.05:1]

/* [Opening 2: spare (audio in, later)] */
o2_on = false;
// Name cut into the wall under it
o2_name = "AUDIO";
o2_wall = "right"; // [left, right, front, back]
// cable, round or rect, as for opening 1 (6.2 mm round = a 3.5 mm panel jack's M6 bush)
o2_shape = "round"; // [cable, round, rect]
// Centre along the wall, mm from the board's left (front/back walls) or front (left/right walls) edge
o2_along = 10;    // [0:0.5:150]
// Centre height above the top of the board
o2_z = 6;         // [-10:0.5:40]
// Width or diameter
o2_w = 6.2;       // [1:0.1:30]
// Height (rect only)
o2_h = 6.2;       // [1:0.1:30]
// Size of the jack's body behind the wall (width, height; 0 = nothing behind the wall); the lid's skirt is cut back to clear it
o2_body_w = 10;   // [0:0.5:30]
o2_body_h = 10;   // [0:0.5:30]

/* [Opening 3: spare] */
o3_on = false;
// Name cut into the wall under it
o3_name = "";
o3_wall = "back"; // [left, right, front, back]
// cable, round or rect, as for opening 1
o3_shape = "rect"; // [cable, round, rect]
// Centre along the wall, mm from the board's left (front/back walls) or front (left/right walls) edge
o3_along = 35;    // [0:0.5:150]
// Centre height above the top of the board
o3_z = 6;         // [-10:0.5:40]
// Width or diameter
o3_w = 10;        // [1:0.1:30]
// Height (rect only)
o3_h = 6;         // [1:0.1:30]
// Size of anything mounted behind the wall (width, height; 0 = nothing)
o3_body_w = 0;    // [0:0.5:30]
o3_body_h = 0;    // [0:0.5:30]

/* [Hidden] */
// More openings, if three are not enough:
// [name, wall, shape, along, z above board, w, h] or with a body: [..., body_w, body_h]
extra_openings = [];
// More holes in the lid: [x, y, diameter] in board coordinates
lid_holes = [];
font = "Liberation Sans:style=Bold";

$fa = 4;
$fs = 0.25;
eps = 0.01;

// Pico 2 board (datasheet section 3: 51 x 21 x 1 mm, 2.54 mm pins, 2.1 mm holes)
pico_len = 51;
pico_wid = 21;
pico_t = 1.0;
pico_pin_end = 1.37;   // first pin from the short edge: (51 - 19 * 2.54) / 2
pico_pin_edge = 1.61;  // pin row from the long edge: (21 - 7 * 2.54) / 2
pico_hole_p = 2.0;
pico_hole_q = 5.7;
usb_overhang = 1.3;    // micro-USB socket past the Pico's edge
usb_rec_l = 5.5;
usb_rec_w = 8.0;
usb_rec_h = 2.7;
usb_center_above_pcb = 1.4;
plug_gap = 0.8;        // plug body to socket face when the plug is all the way in

// ---------------------------------------------------------------- derived

g = board_gap;
L = board_l;
W = board_w;
zf_top = -below;
zf_bot = -below - floor_t;
z_rim = board_t + above;
z_top = z_rim + lid_t;
r_in = max(corner_r - wall, 0.4);
pin_d = hole_d - pin_clear;
holes = [[hole_inset_x, hole_inset_y], [L - hole_inset_x, hole_inset_y],
         [hole_inset_x, W - hole_inset_y], [L - hole_inset_x, W - hole_inset_y]];
gx0 = (L - (grid_cols - 1) * pitch) / 2;
gy0 = (W - (grid_rows - 1) * pitch) / 2;
outer_l = L + 2 * g + 2 * wall;
outer_w = W + 2 * g + 2 * wall;

function across(w) = w == "left" || w == "right";
function dirv(w) = w == "left" ? [-1, 0] : w == "right" ? [1, 0] : w == "front" ? [0, -1] : [0, 1];

pico_D = dirv(pico_usb_wall);
pico_p = -pico_D;                    // into the Pico, away from its USB end
pico_q = [-pico_D[1], pico_D[0]];    // toward pin 1 (left of the USB, seen from above)
pico_ins = pico_inset >= 0 ? pico_inset
         : (across(pico_usb_wall) ? gx0 : gy0) + pico_inset_holes * pitch - pico_pin_end;
pico_ctr = pico_center >= 0 ? pico_center : (across(pico_usb_wall) ? W / 2 : L / 2);
pico_o = pico_usb_wall == "left"  ? [pico_ins, pico_ctr]
       : pico_usb_wall == "right" ? [L - pico_ins, pico_ctr]
       : pico_usb_wall == "front" ? [pico_ctr, pico_ins]
       :                            [pico_ctr, W - pico_ins];
pico_z0 = board_t + pico_height;
function pico_xy(p, q) = pico_o + p * pico_p + q * pico_q;

usb_z = pico_height + pico_t + usb_center_above_pcb;   // above the board's top
// An opening: [name, wall, shape, along, z above the board's top, w, h, body w, body h]
usb_opening = ["USB", pico_usb_wall, "rect", pico_ctr, usb_z, usb_hole_w, usb_hole_h, 0, 0];
user_openings = [
    if (o1_on) [o1_name, o1_wall, o1_shape, o1_along, o1_z, o1_w, o1_h, 0, 0],
    if (o2_on) [o2_name, o2_wall, o2_shape, o2_along, o2_z, o2_w, o2_h, o2_body_w, o2_body_h],
    if (o3_on) [o3_name, o3_wall, o3_shape, o3_along, o3_z, o3_w, o3_h, o3_body_w, o3_body_h],
];
openings = concat([usb_opening], user_openings, extra_openings);

// Local frame on a wall: u along the wall (left to right seen from outside),
// v up, n out of the box; origin on the inside face of the wall.
function wall_m(w, a, z) =
    w == "front" ? [[1, 0, 0, a], [0, 0, -1, -g], [0, 1, 0, z], [0, 0, 0, 1]]
  : w == "back"  ? [[-1, 0, 0, a], [0, 0, 1, W + g], [0, 1, 0, z], [0, 0, 0, 1]]
  : w == "left"  ? [[0, 0, -1, -g], [-1, 0, 0, a], [0, 1, 0, z], [0, 0, 0, 1]]
  :                [[0, 0, 1, L + g], [1, 0, 0, a], [0, 1, 0, z], [0, 0, 0, 1]];

module at_wall(w, a, z) multmatrix(wall_m(w, a, z)) children();

// Local frame on the Pico: x in from its USB end, y away from pin 1, z up from its underside
module pico_frame()
    multmatrix([[pico_p[0], -pico_q[0], 0, pico_o[0]],
                [pico_p[1], -pico_q[1], 0, pico_o[1]],
                [0, 0, 1, pico_z0], [0, 0, 0, 1]]) children();

// ---------------------------------------------------------------- 2D shapes

module rsq(x, y, r) {
    r2 = max(min(r, x / 2 - eps, y / 2 - eps), eps);
    translate([r2, r2]) offset(r = r2) square([x - 2 * r2, y - 2 * r2]);
}

module rsq_c(x, y, r) translate([-x / 2, -y / 2]) rsq(x, y, r);

// The inside of the box, grown by d
module cav2d(d = 0) translate([-g - d, -g - d]) rsq(L + 2 * g + 2 * d, W + 2 * g + 2 * d, r_in + d);

// The outside of the box, grown by d
module out2d(d = 0) translate([-g - wall - d, -g - wall - d])
    rsq(outer_l + 2 * d, outer_w + 2 * d, corner_r + d);

function o_name(o) = o[0];
function o_wall(o) = o[1];
function o_shape(o) = o[2];
function o_along(o) = o[3];
function o_zabs(o) = board_t + o[4];
function o_w(o) = o[5];
function o_h(o) = o_shape(o) == "rect" ? o[6] : o[5];
function o_bw(o) = len(o) > 8 ? o[7] : 0;
function o_bh(o) = len(o) > 8 ? o[8] : 0;
function has_body(o) = o_bw(o) > 0 && o_bh(o) > 0;
function slot_w(o) = o_w(o) + 0.2;
function o_label(o) = len(o_name(o)) > 0 ? o_name(o) : str("unnamed ", o_shape(o));
// Width along the wall, and lowest and highest point (above the board's top),
// of an opening together with its body
function o_span(o) = max(o_shape(o) == "cable" ? slot_w(o) : o_w(o), o_bw(o));
function o_v0(o) = o[4] - max(o_h(o), o_bh(o)) / 2;
function o_v1(o) = o_shape(o) == "cable" ? above : o[4] + max(o_h(o), o_bh(o)) / 2;

// An opening's outline in its wall frame, grown by d
module opening2d(o, d = 0) {
    s = o_shape(o);
    if (s == "round") {
        circle(d = o_w(o) + 2 * d);
    } else if (s == "rect") {
        r = o_name(o) == "USB" ? usb_hole_r : min(1.5, o_w(o) / 4, o_h(o) / 4);
        rsq_c(o_w(o) + 2 * d, o_h(o) + 2 * d, r + d);
    } else if (s == "cable") {
        circle(d = slot_w(o) + 2 * d);
        translate([-slot_w(o) / 2 - d, 0]) square([slot_w(o) + 2 * d, z_rim - o_zabs(o) + 5]);
    }
}

// ---------------------------------------------------------------- the box

module box_world() {
    difference() {
        union() {
            difference() {
                // shell, with a 0.6 mm chamfer round the bottom
                hull() {
                    translate([0, 0, zf_bot + 0.6]) linear_extrude(z_rim - zf_bot - 0.6) out2d();
                    translate([0, 0, zf_bot]) linear_extrude(0.6 + eps) out2d(-0.6);
                }
                translate([0, 0, zf_top]) linear_extrude(z_rim - zf_top + 1) cav2d();
                // lead-in for the lid
                hull() {
                    translate([0, 0, z_rim - 0.5]) linear_extrude(eps) cav2d();
                    translate([0, 0, z_rim]) linear_extrude(1) cav2d(0.5);
                }
            }
            corner_posts();
            if (edge_stops) edge_ribs();
        }
        for (o = openings) opening_cut(o);
        if (mount == "screws")
            for (h = holes) translate([h[0], h[1], zf_top + 0.6]) cylinder(d = screw_pilot_d, h = below);
        if (snap) for (w = ["front", "back"]) snap_groove(w);
        pd = min(1.0, wall - 1.2);   // fingernail notch depth, leaving at least 1.2 mm of wall
        if (pry_notch && pd >= 0.4)
            translate([L * 0.17 - 5, -g - wall - 1, z_rim - 1.2]) cube([10, 1 + pd, 3]);
        if (wall_labels) for (o = openings) wall_label(o);
    }
}

module corner_posts() {
    for (h = holes) {
        // post under the corner, merged into the walls when it is near them
        translate([0, 0, zf_top - eps]) linear_extrude(below + eps) hull() {
            translate(h) circle(d = post_d);
            if (hole_inset_x <= post_d && hole_inset_y <= post_d)
                translate([h[0] < L / 2 ? -g - 1 : L + g, h[1] < W / 2 ? -g - 1 : W + g]) square(1);
        }
        if (mount == "pins") translate([h[0], h[1], -eps]) peg();
    }
}

module peg() {
    cylinder(d = pin_d, h = board_t + pin_above - 0.4 + eps);
    translate([0, 0, board_t + pin_above - 0.4]) cylinder(d1 = pin_d, d2 = pin_d - 0.6, h = 0.4);
}

// Ribs beside the board's edges, two per wall, skipped where an opening is
rib_w = 4;
rib_top = board_t + 1;
function rib_spots(w) = let(len_w = across(w) ? W : L) [0.15 * len_w, 0.85 * len_w];
function rib_clear(w, a) = len([for (o = openings)
    if (o_wall(o) == w && abs(o_along(o) - a) < o_span(o) / 2 + rib_w / 2 + 1
        && board_t + o_v0(o) < rib_top + 1) 1]) == 0;

module edge_ribs() {
    dn = g - edge_stop_gap;
    if (dn > 0.05) for (w = ["left", "right", "front", "back"], a = rib_spots(w)) if (rib_clear(w, a))
        at_wall(w, a, zf_top - eps) hull() {   // 45 degree top so the board drops in
            translate([-rib_w / 2, 0, -dn]) cube([rib_w, rib_top - zf_top - dn, dn + eps]);
            translate([-rib_w / 2, 0, -eps]) cube([rib_w, rib_top - zf_top, 2 * eps]);
        }
}

module opening_cut(o) {
    at_wall(o_wall(o), o_along(o), o_zabs(o)) {
        translate([0, 0, -1.5]) linear_extrude(wall + 3) opening2d(o);
        if (o_shape(o) == "cable") {   // lead-in at the rim for the lid's tongue
            sw = slot_w(o);
            vr = z_rim - o_zabs(o);
            translate([0, 0, -1.5]) linear_extrude(wall + 3)
                polygon([[-sw / 2, vr - 0.5], [sw / 2, vr - 0.5], [sw / 2 + 0.5, vr + eps], [-sw / 2 - 0.5, vr + eps]]);
        }
    }
}

// The snap ridge's centre below the rim, kept on the skirt's straight part
bd = min(bump_drop, skirt_h - bump - 0.6);

module snap_groove(w) {
    gd = bump - fit + 0.15;   // groove depth into the wall
    gt = 0.2;                 // the groove is this much taller than the ridge at each edge
    // lowered so that its upper face sits 0.05 mm over the ridge's when the lid is
    // down: the lid can lift 0.05 mm before the ridge holds it
    drop = gd + gt - (bump - fit) - 0.05;
    gl = bump_len + 4;
    at_wall(w, L / 2, z_rim - bd - drop) hull() {
        translate([-gl / 2, -(gd + gt), -0.5]) cube([gl, 2 * (gd + gt), 0.5 + eps]);
        translate([-gl / 2, -gt, gd - eps]) cube([gl, 2 * gt, eps]);
    }
}

// Name of an opening cut into the outside of its wall, under it if there is room
module wall_label(o) {
    s = wall_label_size;
    h = o_h(o);
    v_below = -h / 2 - 1.2 - s / 2;
    v_above = h / 2 + 1.2 + s / 2;
    fits_below = o_zabs(o) + v_below - s / 2 >= zf_bot + 1.0;
    fits_above = o_shape(o) != "cable" && o_zabs(o) + v_above + s / 2 <= z_rim - 0.8;
    if (len(o_name(o)) > 0 && text_depth > 0 && (fits_below || fits_above))
        at_wall(o_wall(o), o_along(o), o_zabs(o))
            translate([0, fits_below ? v_below : v_above, wall - text_depth])
                linear_extrude(text_depth + 1, convexity = 10)
                    text(o_name(o), size = s, font = font, halign = "center", valign = "center");
}

// ---------------------------------------------------------------- the lid

module lid_world() {
    difference() {
        union() {
            // plate, with a 0.8 mm chamfer round the top
            hull() {
                translate([0, 0, z_rim]) linear_extrude(lid_t - 0.8) out2d();
                translate([0, 0, z_top - 0.8 - eps]) linear_extrude(0.8 + eps) out2d(-0.8);
            }
            difference() {
                union() {
                    skirt();
                    if (snap) for (w = ["front", "back"]) snap_bump(w);
                }
                for (o = openings) skirt_notch(o);
            }
            for (o = openings) if (o_shape(o) == "cable") cable_tongue(o);
            if (mount == "pins") for (h = holes) press_post(h);
        }
        lid_text();
        if (led_window) {
            c = pico_xy(pico_led_p, pico_led_q);
            translate([c[0], c[1], z_rim - 1]) cylinder(d = led_window_d, h = lid_t + 2);
        }
        if (bootsel_hole) {
            c = pico_xy(pico_bootsel_p, pico_bootsel_q);
            translate([c[0], c[1], z_rim - 1]) cylinder(d = bootsel_d, h = lid_t + 2);
        }
        for (hh = lid_holes) translate([hh[0], hh[1], z_rim - 1]) cylinder(d = hh[2], h = lid_t + 2);
    }
}

module skirt() {
    z0 = z_rim - skirt_h;
    difference() {
        hull() {   // chamfered bottom edge so the lid finds its way in
            translate([0, 0, z0 + 0.6]) linear_extrude(skirt_h - 0.6 + eps) cav2d(-fit);
            translate([0, 0, z0]) linear_extrude(0.6) cav2d(-fit - 0.6);
        }
        translate([0, 0, z0 - 1]) linear_extrude(skirt_h + 2) cav2d(-fit - skirt_t);
    }
}

module snap_bump(w) {
    at_wall(w, L / 2, z_rim - bd) hull() {
        translate([-bump_len / 2, -bump, -fit - 0.5]) cube([bump_len, 2 * bump, 0.5]);
        translate([-bump_len / 2, -eps, -fit + bump - eps]) cube([bump_len, 2 * eps, eps]);
    }
}

// The skirt is cut back round each opening, and round anything mounted behind it
module skirt_notch(o) {
    at_wall(o_wall(o), o_along(o), o_zabs(o))
        translate([0, 0, -(fit + skirt_t + bump + 1)])
            linear_extrude(fit + skirt_t + bump + 1 + eps) offset(r = 0.5) {
                opening2d(o);
                if (has_body(o)) rsq_c(o_bw(o), o_bh(o), 0.5);
            }
}

// Fills a "cable" slot above the cable and holds the cable down in it
module cable_tongue(o) {
    tw = slot_w(o) - 2 * fit;                // tongue width
    rs = o_w(o) / 2;                         // saddle radius: the cable's
    cz = -cable_pinch;                       // saddle centre, below the slot's centre
    foot = min(0.45, tw / 4);                // flat at each side of the saddle
    v0 = cz + sqrt(max(rs * rs - pow(tw / 2 - foot, 2), 0));
    vt = z_rim - o_zabs(o) + eps;
    ch = min(0.3, foot * 0.6);               // lead-in on the lower corners
    at_wall(o_wall(o), o_along(o), o_zabs(o)) linear_extrude(wall) difference() {
        polygon([[-tw / 2, v0 + ch], [-tw / 2 + ch, v0], [tw / 2 - ch, v0], [tw / 2, v0 + ch],
                 [tw / 2, vt], [-tw / 2, vt]]);
        translate([0, cz]) circle(r = rs);
    }
}

module press_post(h) {
    z0 = board_t + press_gap;
    translate([h[0], h[1], z0]) difference() {
        cylinder(d = press_d, h = z_rim - z0 + eps);
        translate([0, 0, -eps]) cylinder(d = pin_d + 0.6, h = pin_above - press_gap + 0.5);
    }
}

module lid_text() {
    if (text_depth > 0) {
        // centred on the lid, shifted away from the LED window
        led = pico_xy(pico_led_p, pico_led_q);
        cx = L / 2 + (led[0] < L / 2 ? 5 : -5);
        cy = W / 2;
        translate([cx, cy + (len(lid_label2) > 0 ? lid_label_size * 0.45 : 0), z_top - text_depth])
            linear_extrude(text_depth + 1, convexity = 10)
                text(lid_label, size = lid_label_size, font = font, halign = "center", valign = "center");
        if (len(lid_label2) > 0)
            translate([cx, cy - lid_label_size * 0.85, z_top - text_depth])
                linear_extrude(text_depth + 1, convexity = 10)
                    text(lid_label2, size = lid_label_size * 0.75, font = font,
                         halign = "center", valign = "center");
        if (led_window && len(led_label) > 0) {
            // beside the window, on the pin 1 side (away from the BOOTSEL button)
            t = led + (led_window_d / 2 + 3) * pico_q;
            translate([t[0], t[1], z_top - text_depth]) linear_extrude(text_depth + 1, convexity = 10)
                text(led_label, size = 4, font = font, halign = "center", valign = "center");
        }
    }
}

// A quick print to try the board on the pegs before printing the box: a frame
// with the box's corner posts and pegs
module peg_test() {
    linear_extrude(1.2) difference() {
        translate([-1, -1]) square([L + 2, W + 2]);
        translate([6, 6]) square([L - 12, W - 12]);
    }
    for (h = holes) translate([h[0], h[1], 0]) {
        cylinder(d = post_d, h = 3);
        translate([0, 0, 3 - eps]) peg();
    }
}

// ---------------------------------------------------------------- stand-ins for the pictures

// grid = false leaves out the 432 small holes (the fit checks run much faster)
module board_ghost(grid = true) {
    color("#1e8c3a") difference() {
        cube([L, W, board_t]);
        for (h = holes) translate([h[0], h[1], -1]) cylinder(d = hole_d, h = board_t + 2);
        if (grid) for (i = [0 : grid_cols - 1], j = [0 : grid_rows - 1])
            translate([gx0 + i * pitch, gy0 + j * pitch, -1]) cylinder(d = 1.0, h = board_t + 2, $fn = 8);
    }
}

module pico_ghost() {
    pico_frame() {
        color("#2f7d32") difference() {
            translate([0, -pico_wid / 2, 0]) cube([pico_len, pico_wid, pico_t]);
            for (p = [pico_hole_p, pico_len - pico_hole_p], q = [-pico_hole_q, pico_hole_q])
                translate([p, q, -1]) cylinder(d = 2.1, h = 3);
        }
        color("silver") translate([-usb_overhang, -usb_rec_w / 2, pico_t])
            cube([usb_rec_l, usb_rec_w, usb_rec_h]);
        color("#111") translate([pico_len / 2 - 3.5, -3.5, pico_t]) cube([7, 7, 0.9]);
        color("red") translate([pico_led_p - 0.8, -pico_led_q - 0.4, pico_t]) cube([1.6, 0.8, 0.6]);
        color("white") translate([pico_bootsel_p, -pico_bootsel_q, pico_t]) {
            translate([-1.75, -1.75, 0]) cube([3.5, 3.5, 0.8]);
            cylinder(d = 2.2, h = 1.6);
        }
        if (pico_height > 0) color("#222") for (q = [-1, 1])
            translate([0, q * (pico_wid / 2 - pico_pin_edge) - 1.27, -pico_height])
                cube([pico_len, 2.54, pico_height]);
    }
}

module usb_plug_ghost() {
    face = -usb_overhang - plug_gap;
    zc = pico_t + usb_center_above_pcb;
    pico_frame() {
        color("silver") translate([face, -3.43, zc - 0.9]) cube([6.3, 6.86, 1.8]);
        color("#333") translate([face - usb_plug_len, -usb_plug_w / 2, zc - usb_plug_h / 2])
            cube([usb_plug_len, usb_plug_w, usb_plug_h]);
        color("#333") translate([face - usb_plug_len + eps, 0, zc]) rotate([0, -90, 0])
            cylinder(d = 3.5, h = 20);
    }
}

module cable_ghosts(shrink = 0) {
    for (o = openings) if (o_shape(o) == "cable")
        at_wall(o_wall(o), o_along(o), o_zabs(o))
            color("#222") translate([0, 0, -12]) cylinder(d = o_w(o) - shrink, h = wall + 12 + 25);
}

// Whatever is mounted behind an opening (a panel jack's body), 15 mm deep
module body_ghosts() {
    for (o = openings) if (has_body(o))
        at_wall(o_wall(o), o_along(o), o_zabs(o))
            color("#555") translate([-o_bw(o) / 2, -o_bh(o) / 2, -15]) cube([o_bw(o), o_bh(o), 15 - 0.02]);
}

module inside_ghosts() {
    board_ghost();
    pico_ghost();
    body_ghosts();
}

// ---------------------------------------------------------------- parts and views

box_color = "#2f6fb0";
lid_color = "#4a8fd6";

module box_part() translate([0, 0, -zf_bot]) box_world();
module lid_part() translate([0, 0, z_top]) rotate([180, 0, 0]) lid_world();

// Everything on the near side of the cut, removed for the section pictures
module cut_away(axis, at) {
    m = 60;
    if (axis == "y") translate([-g - wall - m, at - 200, zf_bot - 1]) cube([outer_l + 2 * m, 200, z_top - zf_bot + 2]);
    else translate([at - 200, -g - wall - m, zf_bot - 1]) cube([200, outer_w + 2 * m, z_top - zf_bot + 2]);
}

// The cut faces, picked out in orange (an exact slice through the children)
module section_slice(axis, at) {
    if (axis == "y")
        translate([0, at - 0.06, 0]) rotate([-90, 0, 0]) linear_extrude(0.1, center = true)
            projection(cut = true) translate([0, 0, -at]) rotate([90, 0, 0]) children();
    else
        translate([at - 0.06, 0, 0]) rotate([0, 90, 0]) linear_extrude(0.1, center = true)
            projection(cut = true) translate([0, 0, -at]) rotate([0, -90, 0]) children();
}

module section_view(axis, at) {
    color(box_color) difference() { box_world(); cut_away(axis, at); }
    color(lid_color) difference() { lid_world(); cut_away(axis, at); }
    color("orange") section_slice(axis, at) { box_world(); lid_world(); }
    if (show_parts) difference() {
        union() { inside_ghosts(); usb_plug_ghost(); cable_ghosts(); }
        cut_away(axis, at);
    }
}

if (part == "box") {
    box_part();
} else if (part == "lid") {
    lid_part();
} else if (part == "print") {
    color(box_color) box_part();
    color(lid_color) translate([0, -(g + wall) - 10, 0]) lid_part();
} else if (part == "peg_test") {
    peg_test();
} else if (part == "assembly") {
    color(box_color) box_world();
    color(lid_color) lid_world();
    if (show_parts) { usb_plug_ghost(); cable_ghosts(); }
} else if (part == "open") {
    color(box_color) box_world();
    if (show_parts) { inside_ghosts(); usb_plug_ghost(); cable_ghosts(); }
} else if (part == "exploded") {
    color(box_color) box_world();
    translate([0, 0, 45]) color(lid_color) lid_world();
    if (show_parts) translate([0, 0, 20]) inside_ghosts();
} else if (part == "section") {
    section_view(across(pico_usb_wall) ? "y" : "x", pico_ctr);
} else if (part == "section_corner") {
    section_view("x", hole_inset_x);
}
// Fit checks: each comes out empty when nothing overlaps. Parts that only rest
// on each other (lid on rim, board on posts) are lifted 0.01 mm first.
else if (part == "check_lid_box") {
    intersection() { box_world(); translate([0, 0, 0.01]) lid_world(); }
} else if (part == "check_board") {
    intersection() { union() { box_world(); lid_world(); } translate([0, 0, 0.01]) board_ghost(false); }
} else if (part == "check_pico") {
    intersection() { union() { box_world(); lid_world(); } pico_ghost(); }
} else if (part == "check_plug") {
    intersection() { union() { box_world(); lid_world(); board_ghost(false); } usb_plug_ghost(); }
} else if (part == "check_cable") {
    // a cable just under the squeezed size passes untouched
    intersection() { union() { box_world(); lid_world(); } cable_ghosts(2 * cable_pinch + 0.05); }
} else if (part == "check_jacks") {
    intersection() { union() { box_world(); lid_world(); board_ghost(false); pico_ghost(); } body_ghosts(); }
}

// ---------------------------------------------------------------- report

function frac_ok(v) = abs(v - round(v)) < 0.05;
pin1 = pico_xy(pico_pin_end, pico_wid / 2 - pico_pin_edge);
pin1_col = (pin1[0] - gx0) / pitch + 1;
pin1_row = (pin1[1] - gy0) / pitch + 1;
pico_top = pico_height + pico_t + usb_rec_h;
usb_face = pico_ins - usb_overhang;  // socket face, mm in from the board edge
plug_bot = pico_height + pico_t + usb_center_above_pcb - usb_plug_h / 2;  // above the board's top
pico_span = across(pico_usb_wall) ? L : W;

echo(str("Box outside: ", outer_l, " x ", outer_w, " x ", z_top - zf_bot, " mm (lid included)"));
echo(str("Room: ", below, " mm under the board, ", above, " mm over it, ",
         above - pico_top, " mm over the Pico's USB socket"));
echo(str("Pico pin 1 in hole column ", pin1_col, " from the left, row ", pin1_row, " from the front",
         pico_inset < 0 ? str(" (pico_inset_holes = ", pico_inset_holes, ", counted from the ", pico_usb_wall, " edge)") : "",
         frac_ok(pin1_col) && frac_ok(pin1_row) ? "" : "  WARNING: Pico pins are off the hole grid"));
echo(str("USB socket face is ", usb_face + g + wall, " mm inside the outside of the ",
         pico_usb_wall, " wall"));
if (usb_face < -g + 0.1) echo("WARNING: the USB socket reaches the wall: move the Pico in");
if (pico_ins + pico_len > pico_span + g - 0.2)
    echo(str("WARNING: the Pico runs ", pico_ins + pico_len - pico_span,
             " mm past the far edge of the board: on a 70 x 50 board its USB can face only the left or right wall"));
if (above - pico_top < 1) echo("WARNING: less than 1 mm over the Pico: raise 'above'");
if (plug_bot < 0.2 && usb_face - plug_gap > 0)
    echo(str("WARNING: the USB plug's body hits the board's edge: raise pico_height to at least ",
             usb_plug_h / 2 - pico_t - usb_center_above_pcb + 0.2,
             " or put the Pico's USB end at the board's edge (pico_inset <= ", usb_overhang + plug_gap, ")"));
for (o = openings) if (o_shape(o) != "cable" && o_v1(o) > above - 1.5)
    echo(str("WARNING: the ", o_label(o), " opening reaches the rim: raise 'above' to at least ", o_v1(o) + 1.5,
             " or lower the opening"));
for (i = [0 : len(openings) - 1], j = [0 : len(openings) - 1]) {
    a = openings[i];
    b = openings[j];
    if (i < j && o_wall(a) == o_wall(b) && abs(o_along(a) - o_along(b)) < (o_span(a) + o_span(b)) / 2 + 2
        && o_v0(a) < o_v1(b) + 1 && o_v0(b) < o_v1(a) + 1)
        echo(str("WARNING: openings ", o_label(a), " and ", o_label(b), " overlap on the ", o_wall(a),
                 " wall: move one (the openings do not move with the Pico)"));
}
if (snap) for (o = openings)
    if ((o_wall(o) == "front" || o_wall(o) == "back")
        && abs(o_along(o) - L / 2) < o_span(o) / 2 + bump_len / 2 + 2.5
        && board_t + o_v1(o) > z_rim - bd - 1.2)
        echo(str("WARNING: the ", o_label(o), " opening cuts the snap ridge on the ", o_wall(o),
                 " wall: move it away from the middle of the wall"));
if (snap && bd < bump_drop) echo(str("Note: snap ridge raised to ", bd, " mm below the rim to stay on the skirt"));
for (h = holes) {
    c = pico_xy(pico_len / 2, 0);
    d = max(abs(h[0] - c[0]) - (across(pico_usb_wall) ? pico_len : pico_wid) / 2,
            abs(h[1] - c[1]) - (across(pico_usb_wall) ? pico_wid : pico_len) / 2);
    if (d < press_d / 2 + 0.3) echo(str("WARNING: corner post at ", h, " is over the Pico"));
}
