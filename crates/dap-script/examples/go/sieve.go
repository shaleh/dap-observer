// Sieve of Eratosthenes with deliberately rich state to watch under a debugger:
// a []bool, a nested map[int][]int, a []int, and loop scalars.
package main

import "fmt"

func first_n_primes(n int) []int {
	isPrime := make([]bool, n+1)
	for i := 2; i <= n; i++ {
		isPrime[i] = true
	}

	// crossedOff[p] lists the multiples eliminated by prime p. A map of slices,
	// so the variable tree has nested containers to expand.
	crossedOff := map[int][]int{}

	for p := 2; p*p <= n; p++ {
		if !isPrime[p] {
			continue
		}
		for multiple := p * p; multiple <= n; multiple += p {
			if isPrime[multiple] {
				isPrime[multiple] = false
				crossedOff[p] = append(crossedOff[p], multiple) // good breakpoint: line 26
			}
		}
	}

	primes := []int{}
	for i := 2; i <= n; i++ {
		if isPrime[i] {
			primes = append(primes, i)
		}
	}

	fmt.Println("crossed off:", crossedOff)
	return primes
}

func main() {
	const n = 50
	primes := first_n_primes(n)
	fmt.Println("primes:", primes)
}
