"""Idiomatic Python: n-body with bodies as lists and precomputed pairs (pyperformance style)."""
import math
from itertools import combinations

PI = 3.141592653589793
SOLAR_MASS = 4 * PI * PI
DAYS_PER_YEAR = 365.24

# [x, y, z, vx, vy, vz, mass]
BODIES = [
    [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, SOLAR_MASS],
    [4.84143144246472090e00, -1.16032004402742839e00, -1.03622044471123109e-01, 1.66007664274403694e-03 * DAYS_PER_YEAR, 7.69901118419740425e-03 * DAYS_PER_YEAR, -6.90460016972063023e-05 * DAYS_PER_YEAR, 9.54791938424326609e-04 * SOLAR_MASS],
    [8.34336671824457987e00, 4.12479856412430479e00, -4.03523417114321381e-01, -2.76742510726862411e-03 * DAYS_PER_YEAR, 4.99852801234917238e-03 * DAYS_PER_YEAR, 2.30417297573763929e-05 * DAYS_PER_YEAR, 2.85885980666130812e-04 * SOLAR_MASS],
    [1.28943695621391310e01, -1.51111514016986312e01, -2.23307578892655734e-01, 2.96460137564761618e-03 * DAYS_PER_YEAR, 2.37847173959480950e-03 * DAYS_PER_YEAR, -2.96589568540237556e-05 * DAYS_PER_YEAR, 4.36624404335156298e-05 * SOLAR_MASS],
    [1.53796971148509165e01, -2.59193146099879641e01, 1.79258772950371181e-01, 2.68067772490389322e-03 * DAYS_PER_YEAR, 1.62824170038242295e-03 * DAYS_PER_YEAR, -9.51592254519715870e-05 * DAYS_PER_YEAR, 5.15138902046611451e-05 * SOLAR_MASS],
]
PAIRS = list(combinations(BODIES, 2))


def advance(dt, steps):
    sqrt = math.sqrt
    for _ in range(steps):
        for b, b2 in PAIRS:
            dx = b[0] - b2[0]
            dy = b[1] - b2[1]
            dz = b[2] - b2[2]
            d2 = dx * dx + dy * dy + dz * dz
            mag = dt / (d2 * sqrt(d2))
            m1 = b[6]
            m2 = b2[6]
            b[3] -= dx * m2 * mag
            b[4] -= dy * m2 * mag
            b[5] -= dz * m2 * mag
            b2[3] += dx * m1 * mag
            b2[4] += dy * m1 * mag
            b2[5] += dz * m1 * mag
        for b in BODIES:
            b[0] += dt * b[3]
            b[1] += dt * b[4]
            b[2] += dt * b[5]


def energy():
    e = 0.0
    for i, b in enumerate(BODIES):
        e += 0.5 * b[6] * (b[3] * b[3] + b[4] * b[4] + b[5] * b[5])
        for b2 in BODIES[i + 1 :]:
            dx = b[0] - b2[0]
            dy = b[1] - b2[1]
            dz = b[2] - b2[2]
            e -= (b[6] * b2[6]) / math.sqrt(dx * dx + dy * dy + dz * dz)
    return e


def offset_momentum():
    px = py = pz = 0.0
    for b in BODIES:
        px += b[3] * b[6]
        py += b[4] * b[6]
        pz += b[5] * b[6]
    BODIES[0][3] = -px / SOLAR_MASS
    BODIES[0][4] = -py / SOLAR_MASS
    BODIES[0][5] = -pz / SOLAR_MASS


def main():
    offset_momentum()
    print(f"{energy():.9f}")
    advance(0.01, 2000000)
    print(f"{energy():.9f}")


main()
