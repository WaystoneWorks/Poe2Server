use poe_game::grid::Grid;
use poe_game::movement::Walkable;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let g = Grid::parse(&std::fs::read(&a[1]).unwrap()).unwrap();
    let (x0, y0, r): (i32, i32, i32) = (a[2].parse().unwrap(), a[3].parse().unwrap(), a[4].parse().unwrap());
    for l in 0..4 {
        println!("layer {l}");
        for y in y0 - r..=y0 + r {
            let s: Vec<String> = (x0 - r..=x0 + r).map(|x| format!("{:x}", g.layer(l, x, y))).collect();
            println!("{y:5} {}", s.join(""));
        }
    }
}
