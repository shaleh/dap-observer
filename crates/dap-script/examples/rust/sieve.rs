// Sieve of Eratosthenes with deliberately rich state to watch under a debugger:
// a Vec<bool>, a nested map of vectors, and loop scalars.

use std::collections::HashMap;

fn first_n_primes(n: usize) -> Vec<usize> {
    let mut is_prime = vec![true; n + 1];
    is_prime[0] = false;
    is_prime[1] = false;

    // crossed_off[p] lists the multiples eliminated by prime p. A map of vectors,
    // so the variable tree has nested containers to expand.
    let mut crossed_off: HashMap<usize, Vec<usize>> = HashMap::new();

    let mut p = 2;
    while p * p <= n {
        if is_prime[p] {
            let mut eliminated: Vec<usize> = Vec::new();
            let mut multiple = p * p;
            while multiple <= n {
                if is_prime[multiple] {
                    is_prime[multiple] = false;
                    eliminated.push(multiple); // good breakpoint
                }
                multiple += p;
            }
            if !eliminated.is_empty() {
                crossed_off.insert(p, eliminated);
            }
        }
        p += 1;
    }

    println!("crossed off: {:?}", crossed_off);
    (2..=n).filter(|&i| is_prime[i]).collect()
}

fn main() {
    let primes = first_n_primes(50);
    println!("primes: {:?}", primes);
}
