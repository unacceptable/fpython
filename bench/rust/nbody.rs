const PI: f64 = 3.141592653589793;
const SOLAR_MASS: f64 = 4.0 * PI * PI;
const DPY: f64 = 365.24;
#[derive(Clone, Copy)]
struct B { x: f64, y: f64, z: f64, vx: f64, vy: f64, vz: f64, m: f64 }
fn energy(b: &[B]) -> f64 {
    let mut e = 0.0;
    for i in 0..b.len() {
        e += 0.5 * b[i].m * (b[i].vx * b[i].vx + b[i].vy * b[i].vy + b[i].vz * b[i].vz);
        for j in i + 1..b.len() {
            let (dx, dy, dz) = (b[i].x - b[j].x, b[i].y - b[j].y, b[i].z - b[j].z);
            e -= b[i].m * b[j].m / (dx * dx + dy * dy + dz * dz).sqrt();
        }
    }
    e
}
fn main() {
    let mut b = vec![
        B { x: 0.0, y: 0.0, z: 0.0, vx: 0.0, vy: 0.0, vz: 0.0, m: SOLAR_MASS },
        B { x: 4.84143144246472090e00, y: -1.16032004402742839e00, z: -1.03622044471123109e-01, vx: 1.66007664274403694e-03 * DPY, vy: 7.69901118419740425e-03 * DPY, vz: -6.90460016972063023e-05 * DPY, m: 9.54791938424326609e-04 * SOLAR_MASS },
        B { x: 8.34336671824457987e00, y: 4.12479856412430479e00, z: -4.03523417114321381e-01, vx: -2.76742510726862411e-03 * DPY, vy: 4.99852801234917238e-03 * DPY, vz: 2.30417297573763929e-05 * DPY, m: 2.85885980666130812e-04 * SOLAR_MASS },
        B { x: 1.28943695621391310e01, y: -1.51111514016986312e01, z: -2.23307578892655734e-01, vx: 2.96460137564761618e-03 * DPY, vy: 2.37847173959480950e-03 * DPY, vz: -2.96589568540237556e-05 * DPY, m: 4.36624404335156298e-05 * SOLAR_MASS },
        B { x: 1.53796971148509165e01, y: -2.59193146099879641e01, z: 1.79258772950371181e-01, vx: 2.68067772490389322e-03 * DPY, vy: 1.62824170038242295e-03 * DPY, vz: -9.51592254519715870e-05 * DPY, m: 5.15138902046611451e-05 * SOLAR_MASS },
    ];
    let (mut px, mut py, mut pz) = (0.0, 0.0, 0.0);
    for x in &b { px += x.vx * x.m; py += x.vy * x.m; pz += x.vz * x.m; }
    b[0].vx = -px / SOLAR_MASS; b[0].vy = -py / SOLAR_MASS; b[0].vz = -pz / SOLAR_MASS;
    println!("{:.9}", energy(&b));
    let n = b.len();
    for _ in 0..2000000 {
        for i in 0..n {
            for j in i + 1..n {
                let (dx, dy, dz) = (b[i].x - b[j].x, b[i].y - b[j].y, b[i].z - b[j].z);
                let d2 = dx * dx + dy * dy + dz * dz;
                let mag = 0.01 / (d2 * d2.sqrt());
                let (mi, mj) = (b[i].m, b[j].m);
                b[i].vx -= dx * mj * mag; b[i].vy -= dy * mj * mag; b[i].vz -= dz * mj * mag;
                b[j].vx += dx * mi * mag; b[j].vy += dy * mi * mag; b[j].vz += dz * mi * mag;
            }
        }
        for x in b.iter_mut() { x.x += 0.01 * x.vx; x.y += 0.01 * x.vy; x.z += 0.01 * x.vz; }
    }
    println!("{:.9}", energy(&b));
}
