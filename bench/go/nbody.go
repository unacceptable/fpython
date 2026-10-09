package main

import (
	"fmt"
	"math"
)

type Body struct{ x, y, z, vx, vy, vz, mass float64 }

const pi = 3.141592653589793
const solarMass = 4 * pi * pi
const daysPerYear = 365.24

func main() {
	b := []*Body{
		{0, 0, 0, 0, 0, 0, solarMass},
		{4.84143144246472090e00, -1.16032004402742839e00, -1.03622044471123109e-01, 1.66007664274403694e-03 * daysPerYear, 7.69901118419740425e-03 * daysPerYear, -6.90460016972063023e-05 * daysPerYear, 9.54791938424326609e-04 * solarMass},
		{8.34336671824457987e00, 4.12479856412430479e00, -4.03523417114321381e-01, -2.76742510726862411e-03 * daysPerYear, 4.99852801234917238e-03 * daysPerYear, 2.30417297573763929e-05 * daysPerYear, 2.85885980666130812e-04 * solarMass},
		{1.28943695621391310e01, -1.51111514016986312e01, -2.23307578892655734e-01, 2.96460137564761618e-03 * daysPerYear, 2.37847173959480950e-03 * daysPerYear, -2.96589568540237556e-05 * daysPerYear, 4.36624404335156298e-05 * solarMass},
		{1.53796971148509165e01, -2.59193146099879641e01, 1.79258772950371181e-01, 2.68067772490389322e-03 * daysPerYear, 1.62824170038242295e-03 * daysPerYear, -9.51592254519715870e-05 * daysPerYear, 5.15138902046611451e-05 * solarMass},
	}
	var px, py, pz float64
	for _, x := range b {
		px += x.vx * x.mass
		py += x.vy * x.mass
		pz += x.vz * x.mass
	}
	b[0].vx, b[0].vy, b[0].vz = -px/solarMass, -py/solarMass, -pz/solarMass
	energy := func() float64 {
		e := 0.0
		for i, x := range b {
			e += 0.5 * x.mass * (x.vx*x.vx + x.vy*x.vy + x.vz*x.vz)
			for _, y := range b[i+1:] {
				dx, dy, dz := x.x-y.x, x.y-y.y, x.z-y.z
				e -= x.mass * y.mass / math.Sqrt(dx*dx+dy*dy+dz*dz)
			}
		}
		return e
	}
	fmt.Printf("%.9f\n", energy())
	for s := 0; s < 2000000; s++ {
		for i, x := range b {
			for _, y := range b[i+1:] {
				dx, dy, dz := x.x-y.x, x.y-y.y, x.z-y.z
				d2 := dx*dx + dy*dy + dz*dz
				mag := 0.01 / (d2 * math.Sqrt(d2))
				x.vx -= dx * y.mass * mag
				x.vy -= dy * y.mass * mag
				x.vz -= dz * y.mass * mag
				y.vx += dx * x.mass * mag
				y.vy += dy * x.mass * mag
				y.vz += dz * x.mass * mag
			}
		}
		for _, x := range b {
			x.x += 0.01 * x.vx
			x.y += 0.01 * x.vy
			x.z += 0.01 * x.vz
		}
	}
	fmt.Printf("%.9f\n", energy())
}
