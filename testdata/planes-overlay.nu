#!/usr/bin/env nu
# planes-overlay.nu
#
# Empirically verify the LOD precondition: does the terminal alpha-blend two
# overlapping kitty-graphics placements by per-pixel alpha?
#
# Proposed LOD scheme stacks a coarse, semi-transparent front layer over a fine
# back layer, lowering resolution by making front pixels transparent so the fine
# layer shows through. That only works if overlapping placements really blend,
# so this probes exactly that in a minimal way.
#
# Layout (160x48 px = 20x6 cells of 8x8px), z-order front-to-back:
#   - back   (z=0): opaque magenta, full canvas
#   - frontL (z=1): 50%-alpha solid blue, displayed over the LEFT half
#   - frontR (z=1): 50%-alpha solid green, displayed over the RIGHT half
# Left half should read blue@50% over magenta; right half green@50% over magenta.
# A fully transparent front pixel would reveal magenta exactly (alpha blend).
#
# Run inside kitty or ghostty (nu >= 0.115):
#   nu testdata/planes-overlay.nu
# You should see two distinct blended halves (blue-purple and green-purple),
# NOT two opaque solids covering everything.

let here = ($env.FILE_PWD)
let target = $"($here)/target/planes-overlay"
mkdir $target

# Generate the three uniform RGBA payloads if not present (320*48*4 = 30720 B).
# F: opaque magenta (ff,00,ff,ff)  B: blue a=128  G: green a=128
if not ($target | path join 'plane_magenta.raw' | path exists) {
  ^bash -c 'for i in $(seq 7680); do printf "\xff\x00\xff\xff"; done > "$0"' $"($target)/plane_magenta.raw"
}
if not ($target | path join 'plane_blue.raw' | path exists) {
  ^bash -c 'for i in $(seq 7680); do printf "\x00\x00\xff\x80"; done > "$0"' $"($target)/plane_blue.raw"
}
if not ($target | path join 'plane_green.raw' | path exists) {
  ^bash -c 'for i in $(seq 7680); do printf "\x00\xff\x00\x80"; done > "$0"' $"($target)/plane_green.raw"
}

let W = 160
let H = 48
let CW = 20
let CH = 6

# back: opaque magenta, full canvas (z=0) -------------------------------------
let back = (open --raw $"($target)/plane_magenta.raw")
print $"\e_Ga=T,f=24,s=($W),v=($H),z=0;($back | encode base64)\e\\"
print $"\e_Ga=p,z=0,c=($CW),r=($CH),C=1\e\\"

# front-left: 50% blue, left half (z=1) ---------------------------------------
let fl = (open --raw $"($target)/plane_blue.raw")
print $"\e_Ga=T,f=24,s=($W),v=($H),z=1;($fl | encode base64)\e\\"
print $"\e_Ga=p,z=1,x=0,y=0,w=($W / 2),h=($H),c=($CW / 2),r=($CH),C=1\e\\"

# front-right: 50% green, right half (z=1) ------------------------------------
let fr = (open --raw $"($target)/plane_green.raw")
print $"\e_Ga=T,f=24,s=($W),v=($H),z=1;($fr | encode base64)\e\\"
print $"\e_Ga=p,z=1,x=($W / 2),y=0,w=($W / 2),h=($H),c=($CW / 2),r=($CH),C=1\e\\"

print "overlay done"