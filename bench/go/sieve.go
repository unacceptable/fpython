package main

import "fmt"

func main() {
	n := 20000000
	flags := make([]bool, n+1)
	for i := range flags {
		flags[i] = true
	}
	flags[0], flags[1] = false, false
	for i := 2; i*i <= n; i++ {
		if flags[i] {
			for j := i * i; j <= n; j += i {
				flags[j] = false
			}
		}
	}
	c := 0
	for _, f := range flags {
		if f {
			c++
		}
	}
	fmt.Println(c)
}
