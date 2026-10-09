fn main() {
    let n = 20000000usize;
    let mut f = vec![true; n + 1];
    f[0] = false; f[1] = false;
    let mut i = 2;
    while i * i <= n {
        if f[i] { let mut j = i * i; while j <= n { f[j] = false; j += i; } }
        i += 1;
    }
    println!("{}", f.iter().filter(|x| **x).count());
}
