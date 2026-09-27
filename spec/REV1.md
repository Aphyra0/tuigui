`crates/`:
- streamer/
- tgp/
- cage/
- src/

the algo for streaming:

we take the existing image/screenshot.

Then split it quadtree,

on every node of the tree we set the size, position and the color. This goes to fixed lowest resolution of the "quad" that is displayed by the protocol (the image we upload).

btw, every node in the tree has corresponding image. it's always fixed resolution. SO say two levels below root will have same res as 4 level deep one. But if displayed through terminal one will look lower res because it's scaled up according to the depth. 

we zip iterate the new tree and the old one and see if previous tree node is different enough from the current one.

if it is - we send that new block (it should be just a lower res version of everything from lower levels combined.)
