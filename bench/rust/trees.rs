struct Node { l: Option<Box<Node>>, r: Option<Box<Node>> }
fn make(d: u32) -> Box<Node> {
    if d == 0 { Box::new(Node { l: None, r: None }) } else { Box::new(Node { l: Some(make(d - 1)), r: Some(make(d - 1)) }) }
}
fn check(n: &Node) -> u64 {
    match (&n.l, &n.r) { (Some(l), Some(r)) => 1 + check(l) + check(r), _ => 1 }
}
fn main() {
    let max = 18;
    let long_lived = make(max);
    let mut total = 0u64;
    let mut d = 4;
    while d <= max {
        let iters = 1u64 << (max - d + 4);
        for _ in 0..iters { total += check(&make(d)); }
        d += 2;
    }
    println!("{} {}", total, check(&long_lived));
}
