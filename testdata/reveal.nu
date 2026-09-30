#!/usr/bin/env nu
# reveal.nu — isolate whether a terminal reveals the sharp layer behind a
# transparent-edited loading layer. Exactly mirrors the encoder's two-layer
# scheme:
#   1. TRANSMIT sharp canvas (z=0), opaque magenta (160x48).
#   2. TRANSMIT loading canvas (z=1), opaque cyan (80x24, scaled down by 2),
#      placed over the same pane.
#   3. a=f CLEAR a rect on the loading root frame to transparent
#      (rgba 0,0,0,0) with X=1, at scaled offset/size (x=0,y=0,w=40,h=24 in
#      loading pixels).
# If the terminal alpha-composites placements by pixel: the left half (cleared)
# shows magenta (sharp), the right half stays cyan (loading still opaque).
# If it does NOT reveal: both halves stay cyan -> the front layer never goes
# away, which is exactly the reported symptom.
#
# Run: nu testdata/reveal.nu   (kitty or ghostty)

let here = ($env.FILE_PWD)
let target = $"($here)/target/reveal"
mkdir $target

def raw01 [ file: string, n: int ] {
  # write N copies of the 4-byte RGBA tuple first given, to file via bash
}

# sharp (magenta): 160*48*4 = 30720 bytes
let sw = 160; let sh = 48
# loading (cyan): 80*24*4 = 7680 bytes
let lw = 80; let lh = 24
let clear_w = 40; let clear_h = 24   # in loading px, left half

^bash -c 'for i in $(seq 7680); do printf "\xff\x00\xff\xff"; done > "$0"' $"($target)/sharp.raw"
^bash -c 'for i in $(seq 1920); do printf "\x00\xff\xff\xff"; done > "$0"' $"($target)/loading.raw"
^bash -c 'for i in $(seq 960);  do printf "\x00\x00\x00\x00"; done > "$0"' $"($target)/clear.raw"

let CW = 20; let CH = 6

# sharp=1, loading=2 (explicit).
let sharp = (open --raw $"($target)/sharp.raw")
let loading = (open --raw $"($target)/loading.raw")
let clear = (open --raw $"($target)/clear.raw")

# transmit sharp (z=0), id=1
print $"\e_Ga=T,i=1,f=32,s=($sw),v=($sh),z=0,C=1;($sharp | encode base64)\e\\"
print $"\e_Ga=p,i=1,z=0,c=($CW),r=($CH),C=1\e\\"

# transmit loading (z=1), id=2, scaled image
print $"\e_Ga=T,i=2,f=32,s=($lw),v=($lh),z=1,C=1;($loading | encode base64)\e\\"
print $"\e_Ga=p,i=2,z=1,c=($CW),r=($CH),C=1\e\\"

# a=f clear left half of loading root frame to transparent
print $"\e_Ga=f,i=2,r=1,x=0,y=0,s=($clear_w),v=($clear_h),X=1;($clear | encode base64)\e\\"

print "reveal done"