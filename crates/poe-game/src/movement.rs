//! What the movement code needs of the terrain. The movement itself is the client's, ported in
//! [`crate::clientmotion`]: the client runs in lockstep and moves an entity only when the server
//! tells it to, then simulates the movement itself, so the server simulates the same.

/// Which grid cells an entity can stand on.
pub trait Walkable {
    fn walkable(&self, x: i32, y: i32) -> bool;

    /// Distance-to-wall value of a cell (0 = blocked; the client's grid has 0..5), used to
    /// find which way a wall runs.
    fn clearance(&self, x: i32, y: i32) -> u8 {
        self.walkable(x, y) as u8
    }

    /// The client's 4-bit cell value of layer 0 (0 wall, 1..4 near a wall, 5 clear), which the
    /// client's pathfinder compares with the entity's clearance level. Terrain without such
    /// values has walls (0) and clear cells (5).
    fn nibble(&self, x: i32, y: i32) -> u8 {
        if self.walkable(x, y) { 5 } else { 0 }
    }

    /// Size of the terrain in cells, if it has one (cells outside are walls).
    fn bounds(&self) -> Option<(i32, i32)> {
        None
    }
}

/// Everything is walkable (no terrain data).
pub struct Open;

impl Walkable for Open {
    fn walkable(&self, _: i32, _: i32) -> bool {
        true
    }
}
