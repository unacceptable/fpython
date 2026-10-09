use std::collections::HashMap;
fn main() {
    let vocab = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa", "lambda", "mu"];
    let mut parts: Vec<String> = Vec::with_capacity(2000000);
    let mut x: i64 = 12345;
    for _ in 0..2000000 {
        x = (x * 1103515245 + 12345) % 2147483648;
        parts.push(format!("{}{}", vocab[(x % vocab.len() as i64) as usize], x % 97));
    }
    let text = parts.join(" ");
    let mut counts: HashMap<&str, i64> = HashMap::new();
    for w in text.split_whitespace() { *counts.entry(w).or_insert(0) += 1; }
    let mut items: Vec<(&str, i64)> = counts.iter().map(|(k, v)| (*k, *v)).collect();
    items.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    println!("{} [('{}', {}), ('{}', {}), ('{}', {})]", counts.len(), items[0].0, items[0].1, items[1].0, items[1].1, items[2].0, items[2].1);
}
