package main

import (
	"fmt"
	"sort"
	"strconv"
	"strings"
)

func main() {
	vocab := []string{"alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa", "lambda", "mu"}
	parts := make([]string, 0, 2000000)
	x := int64(12345)
	for i := 0; i < 2000000; i++ {
		x = (x*1103515245 + 12345) % 2147483648
		parts = append(parts, vocab[x%int64(len(vocab))]+strconv.FormatInt(x%97, 10))
	}
	text := strings.Join(parts, " ")
	counts := map[string]int{}
	for _, w := range strings.Fields(text) {
		counts[w]++
	}
	type kv struct {
		k string
		v int
	}
	items := make([]kv, 0, len(counts))
	for k, v := range counts {
		items = append(items, kv{k, v})
	}
	sort.Slice(items, func(i, j int) bool {
		if items[i].v != items[j].v {
			return items[i].v > items[j].v
		}
		return items[i].k < items[j].k
	})
	fmt.Printf("%d [('%s', %d), ('%s', %d), ('%s', %d)]\n", len(counts), items[0].k, items[0].v, items[1].k, items[1].v, items[2].k, items[2].v)
}
